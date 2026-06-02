//! ODIM_H5 polar-volume (PVOL) reader.
//!
//! Where [`crate::reader`] handles 2-D cartesian `COMP` composites,
//! this module reads **polar volumes**: multi-elevation, multi-moment
//! radar data in native spherical (range × azimuth) coordinates,
//! before any cartesian compositing.
//!
//! ## PVOL HDF5 layout
//!
//! - `/what` — `object` (string; FMI mislabels polar volumes as
//!   `"SCAN"` — accepted regardless), `date`/`time` (UTC stamp),
//!   `source` (comma-separated `TYP:VALUE` tokens — `NOD`/`PLC`/`WMO`
//!   extracted by [`parse_source`]).
//! - `/where` — `lon`/`lat`/`height` (radar antenna position).
//! - `/dataset1` .. `/datasetN` — one group per elevation sweep,
//!   enumerated by probing until `file.group()` errors.
//!   - `/datasetN/where` — `elangle`, `nbins`, `nrays`, `rscale`,
//!     `rstart` (default 0.0), `a1gate` (default 0).
//!   - `/datasetN/data1` .. `/datasetN/dataM` — one group per radar
//!     moment, enumerated by probing until it errors.
//!     - `/datasetN/dataM/what` — `quantity`, `gain`, `offset`,
//!       `nodata`, `undetect`. Missing scaling attributes fall back
//!       to `/datasetN/what` (producer variation).
//!     - `/datasetN/dataM/data` — raw array, shape `{nrays, nbins}`.
//!
//! Milestone 1 is reader + domain model only — nothing here is wired
//! into the engine, `MapEngine`, EDR, or the server.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use hdf5_reader::{Attribute, Hdf5File};

use crate::reader::{RawPixels, ReadError};

/// Radar site metadata parsed from `/where` plus the `/what/source`
/// identifier string.
#[derive(Debug, Clone)]
pub struct RadarSite {
    /// Antenna longitude (degrees east).
    pub lon: f64,
    /// Antenna latitude (degrees north).
    pub lat: f64,
    /// Antenna height above sea level (metres).
    pub height: f64,
    /// ODIM node identifier (`NOD:` token of `/what/source`, e.g.
    /// `"fianj"`). `None` if the token is absent.
    pub nod: Option<String>,
    /// Place name (`PLC:` token, e.g. `"Anjalankoski"`).
    pub plc: Option<String>,
    /// WMO station number (`WMO:` token, e.g. `"02954"`).
    pub wmo: Option<String>,
}

/// A single radar moment (quantity) within one elevation sweep.
///
/// **Metadata only** — the bulky pixel array is *not* held here. A scan
/// parses every moment's scaling/identity attributes (cheap) but defers
/// the `/datasetN/dataM/data` array, which is read lazily on demand via
/// [`read_moment_pixels`] (keyed by [`Self::dataset_path`]) and cached by
/// the engine. This keeps a parsed [`PolarVolume`] at KB scale so a whole
/// radar network's catalog fits in memory; see issue #289.
#[derive(Debug, Clone)]
pub struct PolarMoment {
    /// ODIM quantity name, e.g. `"DBZH"`, `"VRADH"`, `"ZDR"`.
    pub quantity: String,
    /// Linear scale factor: `physical = raw * gain + offset`.
    pub gain: f64,
    /// Linear offset.
    pub offset: f64,
    /// Raw value indicating "no data" / out-of-coverage.
    pub nodata: f64,
    /// Raw value indicating "no echo detected".
    pub undetect: f64,
    /// HDF5 path of this moment's raw data array
    /// (`/datasetN/dataM/data`), for the lazy pixel read. The array is
    /// shape `(nrays, nbins)`, indexed `[ray, bin]`.
    pub dataset_path: String,
}

/// One elevation sweep of a polar volume: all moments measured at a
/// single antenna elevation angle.
#[derive(Debug, Clone)]
pub struct Sweep {
    /// Antenna elevation angle (degrees above horizontal).
    pub elangle: f64,
    /// Number of range bins per ray.
    pub nbins: usize,
    /// Number of azimuth rays in the sweep.
    pub nrays: usize,
    /// Range-bin spacing (metres per bin).
    pub rscale: f64,
    /// Range to the start of the first bin (metres).
    pub rstart: f64,
    /// Index of the first radiated azimuth ray.
    pub a1gate: usize,
    /// Radar moments measured in this sweep.
    pub moments: Vec<PolarMoment>,
}

/// A parsed ODIM polar volume: the radar site plus every elevation
/// sweep, sorted by elevation angle ascending.
#[derive(Debug, Clone)]
pub struct PolarVolume {
    /// Radar antenna position + identifiers.
    pub site: RadarSite,
    /// Nominal acquisition time (UTC), from `/what/date` + `/what/time`.
    pub time: DateTime<Utc>,
    /// Raw `/what/object` value (e.g. `"PVOL"` or — for FMI — `"SCAN"`).
    /// Kept for diagnostics; never used to reject a file.
    pub object: String,
    /// Elevation sweeps, sorted by `elangle` ascending.
    pub sweeps: Vec<Sweep>,
}

/// Parse an ODIM `/what/source` string into `(NOD, PLC, WMO)`.
///
/// The source attribute is a comma-separated list of `TYP:VALUE`
/// tokens, e.g.
/// `"WIGOS:0-246-0-101234,WMO:02954,RAD:FI44,PLC:Anjalankoski,NOD:fianj"`.
/// Tokens whose type is not requested are ignored; a requested type
/// that is absent yields `None`. Surrounding whitespace and HDF5
/// NUL-padding are trimmed.
fn parse_source(source: &str) -> (Option<String>, Option<String>, Option<String>) {
    let mut nod = None;
    let mut plc = None;
    let mut wmo = None;
    for token in source.split(',') {
        let token = token.trim_matches(|c: char| c.is_whitespace() || c == '\0');
        let Some((typ, value)) = token.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match typ.trim() {
            "NOD" => nod = Some(value.to_string()),
            "PLC" => plc = Some(value.to_string()),
            "WMO" => wmo = Some(value.to_string()),
            _ => {}
        }
    }
    (nod, plc, wmo)
}

/// Read a `RawPixels` array of declared shape `(nrays, nbins)` from a
/// `/datasetN/dataM/data` dataset, probing u8 → u16 → f64 — the same
/// dtype-fallback chain [`crate::reader::read_composite`] uses.
fn read_moment_array(
    file: &Hdf5File,
    path: &str,
    nrays: usize,
    nbins: usize,
) -> Result<RawPixels, ReadError> {
    let ds = file
        .dataset(path)
        .map_err(|_| ReadError::MissingGroup(path.to_string()))?;
    let shape = ds.shape();
    if shape.len() != 2 {
        return Err(ReadError::UnsupportedRank(shape.len()));
    }

    let check = |dim: (usize, usize)| -> Result<(), ReadError> {
        if dim != (nrays, nbins) {
            return Err(ReadError::DatasetRead(format!(
                "{path}: array shape {dim:?} doesn't match sweep metadata {nrays}x{nbins}"
            )));
        }
        Ok(())
    };

    if let Ok(arr) = ds.read_array::<u8>() {
        let a2 = arr
            .into_dimensionality::<ndarray::Ix2>()
            .map_err(|e| ReadError::DatasetRead(format!("{path}: u8 reshape failed: {e}")))?;
        check(a2.dim())?;
        return Ok(RawPixels::U8(a2));
    }
    if let Ok(arr) = ds.read_array::<u16>() {
        let a2 = arr
            .into_dimensionality::<ndarray::Ix2>()
            .map_err(|e| ReadError::DatasetRead(format!("{path}: u16 reshape failed: {e}")))?;
        check(a2.dim())?;
        return Ok(RawPixels::U16(a2));
    }
    if let Ok(arr) = ds.read_array::<f64>() {
        let a2 = arr
            .into_dimensionality::<ndarray::Ix2>()
            .map_err(|e| ReadError::DatasetRead(format!("{path}: f64 reshape failed: {e}")))?;
        check(a2.dim())?;
        return Ok(RawPixels::F64(a2));
    }
    Err(ReadError::UnsupportedPixelType)
}

/// Parse an ODIM_H5 polar volume from a byte slice. The whole file
/// must fit in memory — typical PVOL files are 10–30 MB.
///
/// Sweeps are returned sorted by elevation angle ascending. A volume
/// with zero `/datasetN` groups, or a sweep with zero moments, is an
/// error ([`ReadError::NoSweeps`] / [`ReadError::NoMoments`]).
pub fn read_polar_volume(bytes: &[u8]) -> Result<PolarVolume, ReadError> {
    let file = Hdf5File::from_bytes(bytes).map_err(|e| ReadError::OpenFailed(e.to_string()))?;

    // Closure-based attribute helpers — `Group` isn't pub-exported by
    // `hdf5-reader`, so each helper resolves the group by path.
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

    // /what — object kind, timestamp, source identifier. The `object`
    // value is recorded but never used to reject the file: FMI ships
    // polar volumes labelled `"SCAN"` rather than `"PVOL"`.
    let object = read_string_attr("/what", "object")?
        .trim_end_matches('\0')
        .trim()
        .to_string();
    let date = read_string_attr("/what", "date")?;
    let time_str = read_string_attr("/what", "time")?;
    let time = parse_odim_timestamp(&date, &time_str)?;
    let source = read_string_attr("/what", "source").unwrap_or_default();
    let (nod, plc, wmo) = parse_source(&source);

    // /where — radar antenna position.
    let site = RadarSite {
        lon: read_f64_attr("/where", "lon")?,
        lat: read_f64_attr("/where", "lat")?,
        height: read_f64_attr("/where", "height")?,
        nod,
        plc,
        wmo,
    };

    // Enumerate /dataset1, /dataset2, … by probing until a group is
    // absent. ODIM numbers datasets contiguously from 1.
    let mut sweeps = Vec::new();
    let mut n = 1usize;
    loop {
        let ds_path = format!("/dataset{n}");
        if file.group(&ds_path).is_err() {
            break;
        }
        sweeps.push(read_sweep(
            &file,
            &ds_path,
            &read_string_attr,
            &read_f64_attr,
        )?);
        n += 1;
    }
    if sweeps.is_empty() {
        return Err(ReadError::NoSweeps);
    }

    // Sort by elevation angle ascending. ODIM does not mandate that
    // /datasetN groups are stored in elevation order, so sort
    // explicitly. `total_cmp` keeps a stray NaN from poisoning the
    // ordering.
    sweeps.sort_by(|a, b| a.elangle.total_cmp(&b.elangle));

    Ok(PolarVolume {
        site,
        time,
        object,
        sweeps,
    })
}

/// Read one `/datasetN` elevation sweep, including every moment group.
fn read_sweep(
    file: &Hdf5File,
    ds_path: &str,
    read_string_attr: &impl Fn(&str, &str) -> Result<String, ReadError>,
    read_f64_attr: &impl Fn(&str, &str) -> Result<f64, ReadError>,
) -> Result<Sweep, ReadError> {
    let where_path = format!("{ds_path}/where");
    let elangle = read_f64_attr(&where_path, "elangle")?;
    let nbins = read_f64_attr(&where_path, "nbins")? as usize;
    let nrays = read_f64_attr(&where_path, "nrays")? as usize;
    let rscale = read_f64_attr(&where_path, "rscale")?;
    // rstart / a1gate are optional with documented defaults.
    // ODIM_H5 v2.4 Table 4 defines rstart in metres (same unit as
    // rscale) — no km→m scaling, despite older OPERA conventions.
    let rstart = read_f64_attr(&where_path, "rstart").unwrap_or(0.0);
    let a1gate = read_f64_attr(&where_path, "a1gate").unwrap_or(0.0) as usize;

    // Enumerate /datasetN/data1, /data2, … by probing.
    let mut moments = Vec::new();
    let mut m = 1usize;
    loop {
        let data_path = format!("{ds_path}/data{m}");
        if file.group(&data_path).is_err() {
            break;
        }
        moments.push(read_moment(
            file,
            ds_path,
            &data_path,
            nrays,
            nbins,
            read_string_attr,
            read_f64_attr,
        )?);
        m += 1;
    }
    if moments.is_empty() {
        return Err(ReadError::NoMoments {
            dataset: ds_path.to_string(),
        });
    }

    Ok(Sweep {
        elangle,
        nbins,
        nrays,
        rscale,
        rstart,
        a1gate,
        moments,
    })
}

/// Read one `/datasetN/dataM` moment group. `nrays`/`nbins` come from
/// the parent sweep's `/where` and the data array is validated against
/// them.
#[allow(clippy::too_many_arguments)]
fn read_moment(
    file: &Hdf5File,
    ds_path: &str,
    data_path: &str,
    nrays: usize,
    nbins: usize,
    read_string_attr: &impl Fn(&str, &str) -> Result<String, ReadError>,
    read_f64_attr: &impl Fn(&str, &str) -> Result<f64, ReadError>,
) -> Result<PolarMoment, ReadError> {
    let what_path = format!("{data_path}/what");
    let ds_what = format!("{ds_path}/what");

    let quantity = read_string_attr(&what_path, "quantity")
        .map(|q| q.trim_end_matches('\0').trim().to_string())
        .map_err(|_| ReadError::MissingAttribute {
            group: what_path.clone(),
            name: "quantity".into(),
        })?;

    // gain/offset/nodata/undetect: prefer the per-moment what group,
    // fall back to the sweep-level /datasetN/what (producer variation,
    // mirroring read_composite's scaling-attribute fallback).
    let read_scaling = |name: &str, default: Option<f64>| -> Result<f64, ReadError> {
        if let Ok(v) = read_f64_attr(&what_path, name) {
            return Ok(v);
        }
        if let Ok(v) = read_f64_attr(&ds_what, name) {
            return Ok(v);
        }
        default.ok_or_else(|| ReadError::MissingAttribute {
            group: format!("{what_path} | {ds_what}"),
            name: name.to_string(),
        })
    };
    let gain = read_scaling("gain", Some(1.0))?;
    let offset = read_scaling("offset", Some(0.0))?;
    let nodata = read_scaling("nodata", None)?;
    let undetect = read_scaling("undetect", None)?;

    // Validate the data array exists and matches the sweep's declared
    // `(nrays, nbins)` at scan time — cheap (a shape/metadata check, not a
    // decode), so a malformed file is rejected up front rather than only on
    // first render. The array bytes are read lazily via `read_moment_pixels`.
    let dataset_path = format!("{data_path}/data");
    let ds = file
        .dataset(&dataset_path)
        .map_err(|_| ReadError::MissingGroup(dataset_path.clone()))?;
    let shape = ds.shape();
    if shape.len() != 2 {
        return Err(ReadError::UnsupportedRank(shape.len()));
    }
    if shape[0] as usize != nrays || shape[1] as usize != nbins {
        return Err(ReadError::DatasetRead(format!(
            "{dataset_path}: array shape {:?} doesn't match sweep metadata {nrays}x{nbins}",
            (shape[0], shape[1])
        )));
    }

    Ok(PolarMoment {
        quantity,
        gain,
        offset,
        nodata,
        undetect,
        dataset_path,
    })
}

/// Read one moment's raw pixel array on demand from the volume's bytes.
///
/// The lazy companion to [`read_polar_volume`]: re-parse the HDF5 file
/// (cheap — structure only) and decode just the single
/// `/datasetN/dataM/data` dataset named by [`PolarMoment::dataset_path`].
/// `nrays`/`nbins` come from the owning sweep. Used by the engine's
/// bounded pixel cache so the full sweep stack is never resident.
pub fn read_moment_pixels(
    bytes: &[u8],
    dataset_path: &str,
    nrays: usize,
    nbins: usize,
) -> Result<RawPixels, ReadError> {
    let file = Hdf5File::from_bytes(bytes).map_err(|e| ReadError::OpenFailed(e.to_string()))?;
    read_moment_array(&file, dataset_path, nrays, nbins)
}

/// Combine ODIM's split `/what/date` (`YYYYMMDD`) and `/what/time`
/// (`HHMMSS`) into a UTC timestamp.
fn parse_odim_timestamp(date: &str, time: &str) -> Result<DateTime<Utc>, ReadError> {
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
    use std::path::PathBuf;

    /// The canonical FMI `/what/source` string carries all three
    /// identifiers we care about, plus `WIGOS` and `RAD` tokens we
    /// deliberately ignore.
    #[test]
    fn parse_source_extracts_nod_plc_wmo() {
        let src = "WIGOS:0-246-0-101234,WMO:02954,RAD:FI44,PLC:Anjalankoski,NOD:fianj";
        let (nod, plc, wmo) = parse_source(src);
        assert_eq!(nod.as_deref(), Some("fianj"));
        assert_eq!(plc.as_deref(), Some("Anjalankoski"));
        assert_eq!(wmo.as_deref(), Some("02954"));
    }

    /// Tokens that are absent yield `None` rather than an empty string
    /// or an error.
    #[test]
    fn parse_source_missing_tokens_are_none() {
        let (nod, plc, wmo) = parse_source("RAD:FI44,PLC:Korpo");
        assert_eq!(nod, None);
        assert_eq!(plc.as_deref(), Some("Korpo"));
        assert_eq!(wmo, None);
    }

    /// An empty / absent source string yields all `None`.
    #[test]
    fn parse_source_empty_is_all_none() {
        let (nod, plc, wmo) = parse_source("");
        assert!(nod.is_none() && plc.is_none() && wmo.is_none());
    }

    /// HDF5 NUL-padding and stray whitespace around tokens and values
    /// must not leak into the parsed identifiers.
    #[test]
    fn parse_source_trims_padding_and_whitespace() {
        let (nod, plc, wmo) = parse_source(" NOD:fikor , WMO:02949 ,PLC:Korpo\0");
        assert_eq!(nod.as_deref(), Some("fikor"));
        assert_eq!(wmo.as_deref(), Some("02949"));
        assert_eq!(plc.as_deref(), Some("Korpo"));
    }

    /// A token with no `:` separator, or an empty value, is skipped
    /// without poisoning the rest of the parse.
    #[test]
    fn parse_source_skips_malformed_tokens() {
        let (nod, plc, wmo) = parse_source("garbage,NOD:,WMO:02954,PLC:Vimpeli");
        assert_eq!(nod, None, "empty NOD value must yield None");
        assert_eq!(wmo.as_deref(), Some("02954"));
        assert_eq!(plc.as_deref(), Some("Vimpeli"));
    }

    /// End-to-end read of the real FMI Anjalankoski polar volume.
    ///
    /// The 15 MB fixture is **not committed to git**, so the test
    /// skips gracefully when it is absent — CI stays green; a local
    /// checkout with the fixture exercises the full reader.
    #[test]
    fn reads_fmi_anjalankoski_pvol_fixture() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/radar-fmi-pvol/202605150000_fianj_PVOL.h5");
        if !path.exists() {
            eprintln!("skipping reads_fmi_anjalankoski_pvol_fixture: fixture absent at {path:?}");
            return;
        }

        let bytes = std::fs::read(&path).expect("read fixture bytes");
        let vol = read_polar_volume(&bytes).expect("parse FMI PVOL fixture");

        // 13 elevation sweeps, sorted ascending by elevation angle.
        assert_eq!(vol.sweeps.len(), 13, "expected 13 elevation sweeps");
        assert!(
            (vol.sweeps[0].elangle - 0.3).abs() < 0.05,
            "lowest sweep elangle should be ~0.3°, got {}",
            vol.sweeps[0].elangle
        );
        for w in vol.sweeps.windows(2) {
            assert!(
                w[0].elangle <= w[1].elangle,
                "sweeps must be elangle-ascending: {} then {}",
                w[0].elangle,
                w[1].elangle
            );
        }

        // Every sweep has 360 azimuth rays. Range-bin counts vary by
        // elevation (the real FMI volume uses fewer/coarser bins on
        // higher sweeps), so only the lowest sweep is pinned to 500.
        for (i, sweep) in vol.sweeps.iter().enumerate() {
            eprintln!(
                "sweep {i}: elangle={:.2} nrays={} nbins={} moments={}",
                sweep.elangle,
                sweep.nrays,
                sweep.nbins,
                sweep.moments.len()
            );
            assert_eq!(sweep.nrays, 360, "sweep {i} nrays");
            assert!(!sweep.moments.is_empty(), "sweep {i} has moments");
        }
        assert_eq!(vol.sweeps[0].nbins, 500, "lowest sweep nbins");

        // The lowest sweep carries 16 moments including TH and ZDR.
        let sweep0 = &vol.sweeps[0];
        assert_eq!(sweep0.moments.len(), 16, "sweep[0] moment count");
        let quantities: Vec<&str> = sweep0.moments.iter().map(|m| m.quantity.as_str()).collect();
        assert!(
            quantities.contains(&"TH"),
            "sweep[0] should expose TH, got {quantities:?}"
        );
        assert!(
            quantities.contains(&"ZDR"),
            "sweep[0] should expose ZDR, got {quantities:?}"
        );

        // Pixel arrays are read lazily — verify `read_moment_pixels` decodes
        // each moment's `/datasetN/dataM/data` at the declared
        // `(nrays, nbins)` for every sweep/moment pair.
        for sweep in &vol.sweeps {
            for m in &sweep.moments {
                let px = read_moment_pixels(&bytes, &m.dataset_path, sweep.nrays, sweep.nbins)
                    .unwrap_or_else(|e| panic!("read pixels for {}: {e}", m.quantity));
                assert_eq!(
                    px.shape(),
                    (sweep.nrays, sweep.nbins),
                    "moment {} array shape vs sweep grid",
                    m.quantity
                );
            }
        }

        // Radar site identifiers + antenna position.
        assert_eq!(vol.site.nod.as_deref(), Some("fianj"));
        assert!(
            (vol.site.lat - 60.9039).abs() < 0.01,
            "site lat ~60.9039, got {}",
            vol.site.lat
        );
        assert!(
            (vol.site.lon - 27.1081).abs() < 0.01,
            "site lon ~27.1081, got {}",
            vol.site.lon
        );
    }
}
