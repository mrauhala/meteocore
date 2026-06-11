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
//! | SMHI     | v2.2 (PVOL)  | i16        | yes     | **Signed** scaled integers (gain=0.01, sentinels nodata=-32768/undetect=-32767); 32-bit superblock + single DEFLATE chunks — needed both the `i16` storage variant and the patched `hdf5-reader` v1-B-tree chunk-key fix (see root `Cargo.toml`) |
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
//! - Four raw pixel types: u8, u16, i16, f64 (all the variants we've
//!   actually encountered; i16 is SMHI's signed scaled-integer PVOL)
//! - No quality layers, no `how/*` attributes, no PVOL volume data
//!
//! See [[project_odim_engine_plan]] for the full multi-phase plan.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use ds_core::geo::Crs;
use hdf5_reader::{Attribute, Dataset, Datatype, Hdf5File};
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
        "dataset `/dataset1/data1/data` has unsupported pixel type (Phase 1: u8, u16, i16, or f64)"
    )]
    UnsupportedPixelType,
    #[error("dataset read failed: {0}")]
    DatasetRead(String),
    #[error("attribute read failed: {0}")]
    AttributeRead(String),
    #[error("polar volume contains no `/datasetN` elevation sweeps")]
    NoSweeps,
    #[error("elevation sweep `{dataset}` contains no `/dataM` moment groups")]
    NoMoments { dataset: String },
}

/// Raw pixel storage as it appears on disk. ODIM composites/volumes ship
/// one of:
/// - `u8`  — single-byte reflectivity classes (DMI v2.0)
/// - `u16` — extended dynamic range (some EUMETNET v2.x producers, DWD v2.3)
/// - `i16` — **signed** scaled integers (SMHI PVOL: `gain=0.01`, sentinels
///   `nodata=-32768`/`undetect=-32767` — only representable as signed)
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
    I16(Array2<i16>),
    F64(Array2<f64>),
}

/// Classification of a single sampled radar pixel, distinguishing the two
/// kinds of "no value": clear air vs genuinely unmeasured. Returned by
/// [`RawPixels::sample_class`]; the volume voxel-grid sampler uses it to seal
/// an isosurface against clear air without fabricating across the cone of
/// silence (#360).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PixelClass {
    /// A real physical value (`raw * gain + offset`).
    Value(f64),
    /// In-coverage but below detection — the radar looked and saw nothing
    /// (the ODIM `undetect` sentinel). "Clear air".
    Undetect,
    /// Out-of-range index, the ODIM `nodata` sentinel, or a non-finite raw —
    /// genuinely unmeasured / outside coverage.
    Masked,
}

impl RawPixels {
    /// Shape as `(height, width)` — same ordering ndarray uses
    /// internally. The first axis is rows (y), the second is columns
    /// (x), matching HDF5's row-major layout.
    pub fn shape(&self) -> (usize, usize) {
        match self {
            RawPixels::U8(a) => a.dim(),
            RawPixels::U16(a) => a.dim(),
            RawPixels::I16(a) => a.dim(),
            RawPixels::F64(a) => a.dim(),
        }
    }

    /// Read a single pixel at `(row, col)`, returning `None` for the
    /// nodata or undetect sentinels and `Some(physical)` otherwise.
    /// `physical = raw * gain + offset`. Out-of-range indices return
    /// `None`.
    ///
    /// Both `nodata` ("outside coverage") and `undetect` ("radar
    /// looked but saw nothing") are masked here (→ `None`) — neither
    /// renders as a colored pixel. The volume voxel-grid path needs to
    /// tell them apart (to seal an isosurface against clear air without
    /// fabricating across the cone of silence), so it uses
    /// [`Self::sample_class`]; this thin wrapper preserves the
    /// raster/EDR "both masked" behaviour.
    pub fn sample(
        &self,
        row: usize,
        col: usize,
        gain: f64,
        offset: f64,
        nodata: f64,
        undetect: Option<f64>,
    ) -> Option<f64> {
        match self.sample_class(row, col, gain, offset, nodata, undetect) {
            PixelClass::Value(v) => Some(v),
            PixelClass::Undetect | PixelClass::Masked => None,
        }
    }

    /// Read a single pixel at `(row, col)`, **classifying** it as a real
    /// value, clear-air `Undetect` ("the radar looked and saw nothing"), or
    /// `Masked` (out-of-range index, the `nodata` sentinel, or a non-finite
    /// raw — i.e. genuinely unmeasured / outside coverage). `physical =
    /// raw * gain + offset`. The distinction lets a consumer fill clear air
    /// with a finite "no echo" floor while leaving unmeasured cells unknown
    /// (#360).
    pub fn sample_class(
        &self,
        row: usize,
        col: usize,
        gain: f64,
        offset: f64,
        nodata: f64,
        undetect: Option<f64>,
    ) -> PixelClass {
        let raw = match self {
            RawPixels::U8(a) => match a.get((row, col)) {
                Some(v) => *v as f64,
                None => return PixelClass::Masked,
            },
            RawPixels::U16(a) => match a.get((row, col)) {
                Some(v) => *v as f64,
                None => return PixelClass::Masked,
            },
            RawPixels::I16(a) => match a.get((row, col)) {
                Some(v) => *v as f64,
                None => return PixelClass::Masked,
            },
            RawPixels::F64(a) => match a.get((row, col)) {
                Some(v) => *v,
                None => return PixelClass::Masked,
            },
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
        //
        // NaN-aware: some PVOL producers declare `nodata`/`undetect`
        // as NaN. A NaN raw is masked unconditionally below (a NaN
        // physical value is never meaningful radar data), and `raw ==
        // sentinel` is always false for a NaN sentinel (IEEE-754). So
        // with a NaN `undetect`, the `!u.is_nan()` guard skips the
        // undetect check entirely: a clear-air cell (which stores NaN)
        // is classified `Masked` by the raw-is-NaN guard, never
        // `Undetect`. An acceptable edge — integer undetect codes are
        // the norm; a NaN-undetect producer's clear air just won't
        // seal an isosurface (it stays open, like the cone of silence).
        if raw.is_nan() {
            return PixelClass::Masked;
        }
        if !nodata.is_nan() && raw == nodata {
            return PixelClass::Masked;
        }
        if let Some(u) = undetect {
            if !u.is_nan() && raw == u {
                return PixelClass::Undetect;
            }
        }
        PixelClass::Value(raw * gain + offset)
    }

    /// Approximate heap footprint of the backing array, in bytes — element
    /// count times element size. Byte-weights the engine's lazy-pixel LRU.
    pub fn size_bytes(&self) -> usize {
        let (h, w) = self.shape();
        let elem = match self {
            RawPixels::U8(_) => 1,
            RawPixels::U16(_) => 2,
            RawPixels::I16(_) => 2,
            RawPixels::F64(_) => 8,
        };
        h * w * elem
    }
}

/// Decode a 2-D ODIM pixel array into [`RawPixels`], selecting the storage
/// variant from the dataset's **actual** HDF5 datatype rather than probing
/// reader types in fallback order.
///
/// dtype inspection is load-bearing, not stylistic: `hdf5-reader` matches a
/// typed `read_array::<T>()` on element **byte-size only**, not signedness. A
/// signed `i16` moment (SMHI) therefore reads *successfully* — but with every
/// value bit-reinterpreted — as `u16` (e.g. the `undetect` sentinel `-32767`
/// becomes `32769`), so a u16-before-i16 probe order would silently return
/// garbage. Branching on `Datatype` picks the one correct reader.
///
/// Supported ODIM element types: `u8`/`u16`/`i16` scaled integers (physical =
/// `raw * gain + offset`) and `f64` pre-decoded physical values. Anything else
/// (i8, i32, f32, …) is [`ReadError::UnsupportedPixelType`]. `path` only labels
/// error messages.
pub(crate) fn read_raw_pixels_2d(
    ds: &Dataset,
    rows: usize,
    cols: usize,
    path: &str,
) -> Result<RawPixels, ReadError> {
    let shape = ds.shape();
    if shape.len() != 2 {
        return Err(ReadError::UnsupportedRank(shape.len()));
    }

    macro_rules! read_2d {
        ($t:ty, $variant:ident) => {{
            let arr = ds
                .read_array::<$t>()
                .map_err(|e| ReadError::DatasetRead(format!("{path}: {e}")))?;
            let a2 = arr
                .into_dimensionality::<ndarray::Ix2>()
                .map_err(|e| ReadError::DatasetRead(format!("{path}: reshape failed: {e}")))?;
            if a2.dim() != (rows, cols) {
                return Err(ReadError::DatasetRead(format!(
                    "{path}: array shape {:?} doesn't match metadata {rows}x{cols}",
                    a2.dim()
                )));
            }
            RawPixels::$variant(a2)
        }};
    }

    // `byte_order` is intentionally wildcarded: `read_array::<T>()` applies any
    // byte-swapping internally, so the storage variant is fixed by (size, signed)
    // alone. Matching on `signed` is what's load-bearing (the size-only read in
    // hdf5-reader 0.6 would otherwise let a signed array bit-reinterpret as the
    // unsigned reader type).
    let pixels = match ds.dtype() {
        Datatype::FixedPoint {
            size: 1,
            signed: false,
            ..
        } => read_2d!(u8, U8),
        Datatype::FixedPoint {
            size: 2,
            signed: false,
            ..
        } => read_2d!(u16, U16),
        Datatype::FixedPoint {
            size: 2,
            signed: true,
            ..
        } => read_2d!(i16, I16),
        Datatype::FloatingPoint { size: 8, .. } => read_2d!(f64, F64),
        _ => return Err(ReadError::UnsupportedPixelType),
    };
    Ok(pixels)
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
    /// WGS84 bounding box `[west, south, east, north]` — the
    /// envelope of the grid in lon/lat, computed by sampling points
    /// along every edge of `bbox` and inverse-projecting them.
    ///
    /// **Not** the file's raw `LL`/`UR` corner attributes: for a
    /// projected grid (LAEA, stereographic, …) the grid is a
    /// quadrilateral that bows in lon/lat, so its true lon/lat
    /// extent is wider than the LL→UR diagonal. OPERA's LAEA grid,
    /// for instance, reaches ~29° further west at its `UL` corner
    /// than at `LL`. Edge sampling captures that (the same approach
    /// `ds_core::geo::GeoTransform::bbox` uses for GeoTIFF).
    pub wgs84_bbox: [f64; 4],
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

/// Compute the WGS84 envelope `[west, south, east, north]` of a
/// native-CRS `bbox` `[west, south, east, north]` by sampling
/// `EDGE_SAMPLES` points along every edge and inverse-projecting
/// each. Points that fail to reproject are skipped.
///
/// Sampling the edges (not just the four corners) is what captures
/// the bow of a projected grid in lon/lat — the same technique
/// `ds_core::geo::GeoTransform::bbox` uses for GeoTIFF, so the two
/// raster engines report consistent extents for the same data.
fn wgs84_envelope(crs: &Crs, bbox: [f64; 4]) -> [f64; 4] {
    /// Samples per edge. 20 matches `GeoTransform::bbox`; enough to
    /// pin the bow of a continental LAEA grid to well under a pixel.
    const EDGE_SAMPLES: usize = 20;

    let [w, s, e, n] = bbox;
    let mut min_lon = f64::MAX;
    let mut max_lon = f64::MIN;
    let mut min_lat = f64::MAX;
    let mut max_lat = f64::MIN;
    let mut accumulate = |lon: f64, lat: f64| {
        min_lon = min_lon.min(lon);
        max_lon = max_lon.max(lon);
        min_lat = min_lat.min(lat);
        max_lat = max_lat.max(lat);
    };
    for i in 0..=EDGE_SAMPLES {
        let frac = i as f64 / EDGE_SAMPLES as f64;
        // Top + bottom edges (x varies, y pinned).
        let x = w + frac * (e - w);
        for &y in &[s, n] {
            if let Some((lon, lat)) = crs.inverse(x, y) {
                accumulate(lon, lat);
            }
        }
        // Left + right edges (y varies, x pinned).
        let y = s + frac * (n - s);
        for &x in &[w, e] {
            if let Some((lon, lat)) = crs.inverse(x, y) {
                accumulate(lon, lat);
            }
        }
    }

    // If every reprojection failed (a pathological CRS), fall back
    // to a conservative global extent. Returning the native `bbox`
    // here would be a unit-confusion bug: for a projected CRS it is
    // in metres, and the caller expects WGS84 degrees. `[-180, -90,
    // 180, 90]` is over-broad but at least dimensionally valid.
    // (Unreachable for any real grid — `Crs::Wgs84::inverse` is the
    // identity and projected inverses succeed near the grid.)
    if min_lon > max_lon {
        return [-180.0, -90.0, 180.0, 90.0];
    }
    [min_lon, min_lat, max_lon, max_lat]
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

    // /where — projection + grid extents. We need the UL corner as
    // the anchor for the native `bbox` (built further down). ODIM
    // v2.4 §6.1.2 makes all four corner pairs mandatory, but some
    // older producers ship only LL+UR for the axis-aligned
    // lon/lat case; for those (`Crs::Wgs84` only) synthesise UL as
    // `(LL_lon, UR_lat)`. On a projected grid that synthesis is
    // wrong — the UL corner's lon/lat differs materially from LL's
    // because the projection bows the parallels — so refuse it and
    // fail loudly rather than anchor the bbox at a corrupt point.
    let projdef = read_string_attr("/where", "projdef")?;
    let crs = proj::parse(&projdef)?;
    let ul_lon = match read_f64_attr("/where", "UL_lon") {
        Ok(v) => v,
        Err(_) if matches!(crs, Crs::Wgs84) => read_f64_attr("/where", "LL_lon")?,
        Err(_) => {
            return Err(ReadError::MissingAttribute {
                group: "/where".into(),
                name: "UL_lon (mandatory on projected grids)".into(),
            });
        }
    };
    let ul_lat = match read_f64_attr("/where", "UL_lat") {
        Ok(v) => v,
        Err(_) if matches!(crs, Crs::Wgs84) => read_f64_attr("/where", "UR_lat")?,
        Err(_) => {
            return Err(ReadError::MissingAttribute {
                group: "/where".into(),
                name: "UL_lat (mandatory on projected grids)".into(),
            });
        }
    };

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

    // WGS84 envelope: sample every edge of the native `bbox` and
    // inverse-project. The LL→UR diagonal alone under-reports a
    // projected grid's lon/lat extent (see `wgs84_bbox` docs).
    let wgs84_bbox = wgs84_envelope(&crs, bbox);
    tracing::debug!("ODIM WGS84 envelope: {:?}", wgs84_bbox);

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

    // Decode the raw pixel array, selecting the storage type from the
    // dataset's actual HDF5 datatype: u8 (DMI v2.0), u16 (DWD v2.3), i16
    // (SMHI — signed scaled integers), or f64 (OPERA v2.4 — pre-decoded
    // physical values). See [`read_raw_pixels_2d`] for why dtype inspection
    // (not type-probing) is required.
    let pixels = read_raw_pixels_2d(&ds, rows, cols, "/dataset1/data1/data")?;

    Ok(OdimComposite {
        crs,
        xsize,
        ysize,
        bbox,
        wgs84_bbox,
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

    /// `wgs84_envelope` must capture the lon/lat bow of a projected
    /// grid, not the LL→UR corner diagonal — and not even just the
    /// four-corner envelope. Regression for the OPERA LAEA case:
    ///
    /// - the grid's `UL` corner reaches ~39.5° W while `LL` is only
    ///   at ~10.4° W, so an LL→UR diagonal under-reports the west
    ///   edge by ~29°;
    /// - the grid's *top edge* bows to ~73.9° N — ~6° past both the
    ///   `UL` (67.0°) and `UR` (67.6°) corners — so even a
    ///   four-corner envelope under-reports the north edge.
    ///   Per-edge sampling is what catches this.
    ///
    /// Inputs reproduce the real OPERA composite:
    ///   projdef `+proj=laea +lat_0=55 +lon_0=10 +x_0=1950000
    ///            +y_0=-2100000 +ellps=WGS84`
    ///   grid    3800×4400 at 1000 m, UL anchored at the projected
    ///           origin → native bbox `[0, -4.4e6, 3.8e6, 0]` m.
    /// Verified against a live server: envelope ≈
    ///   `[-39.536, 31.746, 57.812, 73.922]`.
    #[test]
    fn wgs84_envelope_captures_laea_trapezoid() {
        let crs = Crs::LambertAzimuthalEqualArea {
            lat0: 55.0_f64.to_radians(),
            lon0: 10.0_f64.to_radians(),
            false_e: 1_950_000.0,
            false_n: -2_100_000.0,
        };
        let native_bbox = [0.0, -4_400_000.0, 3_800_000.0, 0.0];
        let [w, s, e, n] = wgs84_envelope(&crs, native_bbox);

        assert!(
            w < e && s < n,
            "envelope must be well-formed: {w},{s},{e},{n}"
        );
        // The LL→UR shortcut would report west ≈ -10.4 (LL_lon); the
        // true western reach is the UL corner near -39.5° W.
        assert!(
            (-41.0..-38.0).contains(&w),
            "west should reach the UL corner (~-39.5°), got {w}"
        );
        assert!(
            (56.0..60.0).contains(&e),
            "east should reach the UR corner (~57.8°), got {e}"
        );
        assert!(
            (30.0..33.0).contains(&s),
            "south should sit near the LL/LR edge (~31.7°), got {s}"
        );
        // The corner shortcut (or a four-corner envelope) would
        // report north ≈ 67.6° (UR_lat); edge sampling finds the
        // top edge's ~73.9° bow.
        assert!(
            n > 72.0,
            "north should capture the top-edge bow (~73.9°), got {n}"
        );
    }

    /// For a `Wgs84` (axis-aligned lon/lat) grid the envelope is just
    /// the native bbox — `Crs::Wgs84::inverse` is the identity, so no
    /// distortion is introduced.
    #[test]
    fn wgs84_envelope_is_identity_for_wgs84_grid() {
        let bbox = [10.0, 50.0, 25.0, 60.0];
        let env = wgs84_envelope(&Crs::Wgs84, bbox);
        for (got, want) in env.iter().zip(bbox.iter()) {
            assert!((got - want).abs() < 1e-9, "got {env:?}, want {bbox:?}");
        }
    }

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

    /// `sample_class` keeps the value/undetect/nodata distinction that `sample`
    /// flattens (#360): a real value, clear-air `Undetect`, and genuinely
    /// unmeasured `Masked` (nodata sentinel + out-of-range index) are separate.
    #[test]
    fn raw_pixels_sample_class_distinguishes_undetect_from_nodata() {
        // [value, value, nodata, undetect]
        let arr =
            Array2::from_shape_vec((1, 4), vec![5.0_f64, 25.0, -9999000.0, -8888000.0]).unwrap();
        let px = RawPixels::F64(arr);
        let nodata = -9999000.0;
        let undetect = Some(-8888000.0);

        assert_eq!(
            px.sample_class(0, 0, 1.0, 0.0, nodata, undetect),
            PixelClass::Value(5.0)
        );
        assert_eq!(
            px.sample_class(0, 2, 1.0, 0.0, nodata, undetect),
            PixelClass::Masked,
            "nodata → Masked (genuinely unmeasured)"
        );
        assert_eq!(
            px.sample_class(0, 3, 1.0, 0.0, nodata, undetect),
            PixelClass::Undetect,
            "undetect → Undetect (clear air)"
        );
        // Out-of-range index is Masked, not Undetect.
        assert_eq!(
            px.sample_class(0, 99, 1.0, 0.0, nodata, undetect),
            PixelClass::Masked
        );
        // A NaN raw is Masked regardless of sentinels.
        let nanpx = RawPixels::F64(Array2::from_shape_vec((1, 1), vec![f64::NAN]).unwrap());
        assert_eq!(
            nanpx.sample_class(0, 0, 1.0, 0.0, nodata, undetect),
            PixelClass::Masked
        );
        // And `sample` still flattens Undetect + Masked to None (unchanged).
        assert_eq!(px.sample(0, 3, 1.0, 0.0, nodata, undetect), None);
        assert_eq!(px.sample(0, 2, 1.0, 0.0, nodata, undetect), None);
    }
}
