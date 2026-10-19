//! BUFR → station reports. The **only** module that touches `ds_bufr`, so
//! the decoder can be vendored or replaced in one place if a real feed hits
//! one of its gaps (operators 207/22x, 203 in compressed data).
//!
//! Extraction is template-agnostic: it walks the decoded element stream and
//! picks values by Table B descriptor, tracking the period context
//! (`004023`/`004024`/`004025`) that qualifies accumulations, extremes and
//! gusts. The v1 rule for repeated elements is **first non-missing occurrence
//! wins** per `(descriptor, period)` — in 307096 the 2 m air temperature comes
//! before the sensor-height replicas, and precipitation / extremes each carry
//! their own preceding period. Documented in `CLAUDE.md`.

use std::collections::hash_map::{Entry, HashMap};
use std::io::Cursor;
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
pub use ds_bufr::XY;
use ds_bufr::{DataEvent, DataReader, DataSpec, HeaderSections, TableBEntry, Tables, Value};

/// Table B id as `FXXYYY` digits (F is always 0 for elements).
pub fn xy_from_code(code: &str) -> Option<XY> {
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) || !code.starts_with('0') {
        return None;
    }
    let x: u8 = code[1..3].parse().ok()?;
    let y: u8 = code[3..6].parse().ok()?;
    Some(XY { x, y })
}

pub fn code_of(xy: XY) -> String {
    format!("0{:02}{:03}", xy.x, xy.y)
}

/// One decoded element with its period context.
#[derive(Debug, Clone, PartialEq)]
pub struct Element {
    pub xy: XY,
    pub value: f64,
    /// Length of the qualifying period in hours (`None` = instantaneous /
    /// no period seen). Negative BUFR periods ("the 12 h ending at the
    /// nominal time") are stored as their magnitude.
    pub period_hours: Option<f64>,
}

/// One station report (one BUFR subset).
#[derive(Debug, Clone, PartialEq)]
pub struct ObsReport {
    /// WIGOS id (`0-20000-0-02598`), else `0-20000-0-{block}{station:03}`
    /// from the traditional identifiers, else `ship:{callsign}`.
    pub station_id: String,
    pub name: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub elevation: Option<f64>,
    pub time: DateTime<Utc>,
    pub elements: Vec<Element>,
}

/// What kind of failure stopped a message — the bounded `kind` label of
/// `bufr_decode_failures_total` and the key of the WIS2 per-centre summary,
/// so a failure rate can be attributed without reading logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FailureKind {
    /// No `BUFR` magic at all: a NIL bulletin, text, an error page.
    NotBufr,
    /// The reader ran past the end of the data section or message
    /// (`failed to fill whole buffer`): an element read with the wrong
    /// width misaligned the stream, or the message is cut short.
    Truncated,
    /// A descriptor with no table entry, typically an unregistered
    /// national (local) element.
    UnknownDescriptor,
    /// Any other malformed content.
    Invalid,
    /// Valid BUFR using a feature this decoder does not implement.
    Unsupported,
}

impl FailureKind {
    pub const ALL: [FailureKind; 5] = [
        FailureKind::NotBufr,
        FailureKind::Truncated,
        FailureKind::UnknownDescriptor,
        FailureKind::Invalid,
        FailureKind::Unsupported,
    ];

    /// The `kind` metric label.
    pub fn label(self) -> &'static str {
        match self {
            FailureKind::NotBufr => "not_bufr",
            FailureKind::Truncated => "truncated",
            FailureKind::UnknownDescriptor => "unknown_descriptor",
            FailureKind::Invalid => "invalid",
            FailureKind::Unsupported => "unsupported",
        }
    }

    /// The coarser `reason` metric label: `unsupported` or `error`.
    pub fn reason(self) -> &'static str {
        match self {
            FailureKind::Unsupported => "unsupported",
            _ => "error",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("not a BUFR message")]
    NotBufr,
    #[error("bufr: data ends early ({0})")]
    Truncated(String),
    #[error("bufr: {0}")]
    UnknownDescriptor(String),
    #[error("bufr: {0}")]
    Bufr(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
}

impl DecodeError {
    pub fn kind(&self) -> FailureKind {
        match self {
            DecodeError::NotBufr => FailureKind::NotBufr,
            DecodeError::Truncated(_) => FailureKind::Truncated,
            DecodeError::UnknownDescriptor(_) => FailureKind::UnknownDescriptor,
            DecodeError::Bufr(_) => FailureKind::Invalid,
            DecodeError::Unsupported(_) => FailureKind::Unsupported,
        }
    }
}

impl From<ds_bufr::Error> for DecodeError {
    fn from(e: ds_bufr::Error) -> Self {
        match e {
            ds_bufr::Error::NotSupported(m) => DecodeError::Unsupported(m),
            ds_bufr::Error::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                DecodeError::Truncated(e.to_string())
            }
            ds_bufr::Error::Table(m) => DecodeError::UnknownDescriptor(m),
            other => DecodeError::Bufr(other.to_string()),
        }
    }
}

/// Why a subset produced no report (counted, not fatal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    NoStationId,
    NoPosition,
    NoTime,
}

impl SkipReason {
    pub fn label(self) -> &'static str {
        match self {
            SkipReason::NoStationId => "no_station_id",
            SkipReason::NoPosition => "no_position",
            SkipReason::NoTime => "no_time",
        }
    }
}

/// Outcome of decoding one file: reports plus per-subset skips and
/// per-message failures.
#[derive(Debug, Default)]
pub struct Decoded {
    pub reports: Vec<ObsReport>,
    pub skipped: Vec<SkipReason>,
    /// Number of BUFR messages found in the byte stream (decoded or not).
    pub messages: usize,
    /// Messages that failed to decode. A failure is scoped to its own
    /// message: a concatenated file keeps every report from the messages
    /// before and after it (the failing message contributes nothing — its
    /// subsets are only emitted once the whole message has been read).
    pub failed: Vec<DecodeError>,
}

/// A DWD (originating centre 78) local Table B element.
const fn dwd(
    x: u8,
    y: u8,
    element_name: &'static str,
    unit: &'static str,
    scale: i8,
    reference_value: i32,
    bits: u16,
) -> TableBEntry {
    TableBEntry {
        xy: XY { x, y },
        class_name: "local (DWD)",
        element_name,
        unit,
        scale,
        reference_value,
        bits,
    }
}

/// National (local) Table B elements seen in operational WIS2 feeds that
/// the WMO master tables do not carry. A local element has no width in the
/// master table, so without an entry the whole bit stream after it is
/// misaligned and the message fails.
///
/// DWD (centre 78) SYNOP bulletins, each element paired with the first
/// local-table version that defines it. Every entry is identical in every
/// later version through 8, verified against ecCodes 2.47.0
/// `bufr/tables/0/local/<version>/78/0/element.table` (names verbatim).
/// Only elements seen in live `de-dwd` messages are listed.
#[rustfmt::skip]
const DWD_LOCAL_TABLE_B: &[(u8, TableBEntry)] = &[
    (1, dwd(13, 203, "TOTAL WATER EQUIVALENT AT SNOW SAMPLER", "M/M", 2, 0, 7)),
    (1, dwd(20, 203, "FORM OF PRECIPITATION", "Code table", 0, 0, 4)),
    (1, dwd(20, 204, "FALLEN PRECIPITATION (DAY BEFORE)", "Code table", 0, 0, 6)),
    (1, dwd(20, 205, "DEW, RIME ETC. (DAY BEFORE)", "Code table", 0, 0, 7)),
    (1, dwd(20, 206, "OTHER WEATHER (DAY BEFORE)", "Code table", 0, 0, 4)),
    (2, dwd(4, 214, "ACTUAL HOUR OF OBSERVATION", "h", 0, 0, 5)),
    (2, dwd(4, 215, "ACTUAL MINUTE OF OBSERVATION", "min", 0, 0, 6)),
    (2, dwd(20, 193, "ADDITIONAL WEATHER PHENOMENA", "Code table", 0, 0, 7)),
    (2, dwd(20, 194, "HEIGHT OF TOP OF PHENOMENA", "m", -1, -40, 11)),
    (2, dwd(20, 195, "WEATHER PHENOMENA", "Code table", 0, 0, 4)),
    (2, dwd(20, 199, "LOWEST HEIGHT O.BASE O.CLOUD DUR.LAST H.", "m", -1, -40, 11)),
    (6, dwd(12, 195, "SNOW ABOVE GROUND MINIMUM THERM., Y/N", "Code table", 0, 0, 2)),
    (7, dwd(1, 197, "SENSOR INDEX", "Numeric", 0, 0, 5)),
    (7, dwd(11, 202, "MEAN WIND SPEED FROM 6 10MIN-MEASUREMENT", "m/s", 1, 0, 12)),
    (7, dwd(12, 202, "MEAN TEMPERATURE AT 2M", "K", 2, 0, 16)),
    (7, dwd(20, 230, "CLOUD COVER OF THE HILL - CODE TABLE 50", "Code table", 0, 0, 4)),
    (7, dwd(20, 237, "METEOROLOGICAL OPTICAL RANGE", "m", 0, 0, 18)),
    (7, dwd(20, 238, "MINIMUM METEOROLOGICAL OPTICAL RANGE", "m", 0, 0, 18)),
    (7, dwd(20, 239, "MAXIMUM METEOROLOGICAL OPTICAL RANGE", "m", 0, 0, 18)),
    (7, dwd(52, 210, "REFERENCE NO AND DD OF THE HILL - TAB 32", "Code table", 0, 0, 4)),
    (8, dwd(13, 237, "SNOW COVER", "Code table", 0, 0, 3)),
    (8, dwd(20, 216, "DIRECTION MAIN OCC.OF WEATHER PHEN. TAB.", "Code table", 0, 0, 4)),
];

/// The DWD local-table versions [`DWD_LOCAL_TABLE_B`] covers. A message
/// declaring another version decodes against the master tables only, so a
/// local element in it fails the message rather than guessing a width.
const DWD_LOCAL_VERSIONS: std::ops::RangeInclusive<u8> = 1..=8;

/// A WMO master Table B element as an older master-table version defined it.
const fn wmo(
    x: u8,
    y: u8,
    element_name: &'static str,
    unit: &'static str,
    scale: i8,
    reference_value: i32,
    bits: u16,
) -> TableBEntry {
    TableBEntry {
        xy: XY { x, y },
        class_name: "WMO (older master table version)",
        element_name,
        unit,
        scale,
        reference_value,
        bits,
    }
}

/// Master Table B elements whose width, scale or reference value changed
/// after the master-table version they were encoded with, each paired with
/// the last version that defined it this way. The generated tables are the
/// current version, so a message declaring an older master version (live:
/// it-meteoam, jp-jma and il-ims stations still encode version 13) would
/// otherwise read the newer widths and misalign everything after the first
/// such element. Scoped to SYNOP: the radiation elements of 302045 (in
/// 307080, 307086, …), as ecCodes 2.47.0 `bufr/tables/0/wmo/<version>/
/// element.table` defines them in versions 7–13. The other differences
/// ecCodes records against the current tables (versions before 7, the
/// pre-operational entries of versions 14–18 and 36–37, and the redefined
/// 022191/033066) touch no SYNOP sequence and are left out.
#[rustfmt::skip]
const LEGACY_MASTER_TABLE_B: &[(u8, TableBEntry)] = &[
    (13, wmo(14, 1, "LONG-WAVE RADIATION, INTEGRATED OVER 24 HOURS", "J m-2", -3, -2048, 12)),
    (13, wmo(14, 2, "LONG-WAVE RADIATION, INTEGRATED OVER PERIOD SPECIFIED", "J m-2", -3, -2048, 12)),
    (13, wmo(14, 3, "SHORT-WAVE RADIATION, INTEGRATED OVER 24 HOURS", "J m-2", -3, -2048, 12)),
    (13, wmo(14, 4, "SHORT-WAVE RADIATION, INTEGRATED OVER PERIOD SPECIFIED", "J m-2", -3, -2048, 12)),
    (13, wmo(14, 11, "NET LONG-WAVE RADIATION, INTEGRATED OVER 24 HOURS", "J m-2", -3, -2048, 12)),
    (13, wmo(14, 12, "NET LONG-WAVE RADIATION, INTEGRATED OVER PERIOD SPECIFIED", "J m-2", -3, -2048, 12)),
    (13, wmo(14, 13, "NET SHORT-WAVE RADIATION, INTEGRATED OVER 24 HOURS", "J m-2", -3, -2048, 12)),
    (13, wmo(14, 14, "NET SHORT-WAVE RADIATION, INTEGRATED OVER PERIOD SPECIFIED", "J m-2", -3, -2048, 12)),
    (13, wmo(14, 17, "INSTANTANEOUS LONG-WAVE RADIATION", "W m-2", -3, -2048, 12)),
    (13, wmo(14, 18, "INSTANTANEOUS SHORT-WAVE RADIATION", "W m-2", -3, -2048, 12)),
    (13, wmo(14, 28, "GLOBAL SOLAR RADIATION (HIGH ACCURACY), INTEGRATED OVER PERIOD SPECIFIED", "J m-2", -2, 0, 16)),
    (13, wmo(14, 29, "DIFFUSE SOLAR RADIATION (HIGH ACCURACY), INTEGRATED OVER PERIOD SPECIFIED", "J m-2", -2, 0, 16)),
    (13, wmo(14, 30, "DIRECT SOLAR RADIATION (HIGH ACCURACY), INTEGRATED OVER PERIOD SPECIFIED", "J m-2", -2, 0, 16)),
];

/// Holds the (expensive to build) WMO tables — construct once per engine.
pub struct Decoder {
    /// The current master tables.
    tables: Arc<Tables>,
    /// Master tables as older versions defined them: `(last version, tables)`
    /// in ascending order; a message uses the first whose version is ≥ its
    /// own master-table version.
    legacy_master: Vec<(u8, Arc<Tables>)>,
    /// Master + DWD local entries, indexed by local-table version − 1.
    /// Versions that add no entry share the previous version's tables.
    dwd_tables: Vec<Arc<Tables>>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-subset accumulation state while walking the element stream.
#[derive(Default)]
struct Subset {
    wigos: [Option<String>; 4],
    block: Option<i64>,
    station: Option<i64>,
    callsign: Option<String>,
    name: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    elevation: Option<f64>,
    ymd: [Option<i64>; 6],
    /// Current period context in hours (see module docs).
    period: Option<f64>,
    elements: Vec<Element>,
}

impl Decoder {
    pub fn new() -> Self {
        let tables = Arc::new(Tables::default());
        let mut dwd_tables: Vec<Arc<Tables>> = Vec::new();
        let mut current = Arc::clone(&tables);
        for version in DWD_LOCAL_VERSIONS {
            let added = DWD_LOCAL_TABLE_B
                .iter()
                .filter(|(first, _)| *first == version);
            let mut next: Option<Tables> = None;
            for (_, entry) in added {
                next.get_or_insert_with(|| (*current).clone())
                    .table_b
                    .insert(entry.xy, entry);
            }
            if let Some(next) = next {
                current = Arc::new(next);
            }
            dwd_tables.push(Arc::clone(&current));
        }
        let mut last_versions: Vec<u8> = LEGACY_MASTER_TABLE_B.iter().map(|(v, _)| *v).collect();
        last_versions.sort_unstable();
        last_versions.dedup();
        let legacy_master = last_versions
            .into_iter()
            .map(|version| {
                let mut t = (*tables).clone();
                // A message of `version` predates every change after it.
                for (_, entry) in LEGACY_MASTER_TABLE_B.iter().filter(|(v, _)| *v >= version) {
                    t.table_b.insert(entry.xy, entry);
                }
                (version, Arc::new(t))
            })
            .collect();
        Decoder {
            tables,
            legacy_master,
            dwd_tables,
        }
    }

    /// The tables a message decodes against: the master tables of its
    /// master-table version, or the master tables plus the local entries of
    /// its originating centre and local-table version when that pair has a
    /// verified registry (DWD's feed uses a current master version, so the
    /// two are not combined).
    fn tables_for(&self, centre: u16, master_version: u8, local_version: u8) -> &Tables {
        if centre == 78 && DWD_LOCAL_VERSIONS.contains(&local_version) {
            return &self.dwd_tables[usize::from(local_version - DWD_LOCAL_VERSIONS.start())];
        }
        self.legacy_master
            .iter()
            .find(|(last, _)| master_version <= *last)
            .map_or(&self.tables, |(_, t)| t)
    }

    /// Decode every BUFR message in `bytes` (files may concatenate several
    /// `BUFR…7777` messages; anything between messages is skipped). Only a
    /// stream with no `BUFR` magic at all is an `Err`; a message that fails
    /// is recorded in [`Decoded::failed`] and the scan moves on to the next
    /// one via the section-0 total length.
    pub fn decode(&self, bytes: &[u8]) -> Result<Decoded, DecodeError> {
        let mut out = Decoded::default();
        let mut pos = 0usize;
        while let Some(off) = find_magic(&bytes[pos..]) {
            let start = pos + off;
            if bytes.len() < start + 8 {
                break;
            }
            let total =
                u32::from_be_bytes([0, bytes[start + 4], bytes[start + 5], bytes[start + 6]])
                    as usize;
            let end = if total >= 8 && start + total <= bytes.len() {
                start + total
            } else {
                bytes.len()
            };
            if let Err(e) = self.decode_message(&bytes[start..end], &mut out) {
                out.failed.push(e);
            }
            out.messages += 1;
            pos = end.max(start + 4);
        }
        if out.messages == 0 {
            return Err(DecodeError::NotBufr);
        }
        merge_same_observation(&mut out.reports);
        Ok(out)
    }

    fn decode_message(&self, msg: &[u8], out: &mut Decoded) -> Result<(), DecodeError> {
        let mut reader = Cursor::new(msg);
        let header = HeaderSections::read(&mut reader)?;
        let id = &header.identification_section;
        let tables = self.tables_for(id.centre, id.master_table_version, id.local_tables_version);
        let spec = DataSpec::from_data_description(&header.data_description_section, tables)?;
        let n_subsets = spec.number_of_subsets as usize;
        let mut dr = DataReader::new(&mut reader, &spec)?;

        let mut subsets: Vec<Subset> = Vec::new();
        let mut current: Option<Subset> = None;
        let compressed = spec.is_compressed;
        if compressed {
            subsets.resize_with(n_subsets, Subset::default);
        }
        loop {
            match dr.read_event()? {
                DataEvent::SubsetStart(_) => current = Some(Subset::default()),
                DataEvent::SubsetEnd => {
                    if let Some(s) = current.take() {
                        subsets.push(s);
                    }
                }
                DataEvent::CompressedStart => {}
                DataEvent::Data { xy, value, .. } => {
                    if let Some(s) = current.as_mut() {
                        s.take(xy, &value);
                    }
                }
                DataEvent::CompressedData { xy, values, .. } => {
                    for (s, v) in subsets.iter_mut().zip(values.iter()) {
                        s.take(xy, v);
                    }
                }
                DataEvent::Eof => break,
                _ => {}
            }
        }
        if let Some(s) = current.take() {
            subsets.push(s);
        }
        for s in subsets {
            match s.finish() {
                Ok(r) => out.reports.push(r),
                Err(why) => out.skipped.push(why),
            }
        }
        Ok(())
    }
}

/// Reports of one station and time from several messages of one stream are
/// one observation: a DWD bulletin carries a station's SYNOP and its
/// national supplement (visibility, precipitation, extra weather) as
/// separate messages. Merged here, the later message adds its elements
/// instead of replacing the whole row when the store ingests it. The earlier
/// message's elements stay first, so "first non-missing occurrence wins"
/// keeps its value wherever both carry one. Across separate payloads a
/// later report still replaces the row (a correction).
///
/// Only reports at the same position merge: a SHIP bulletin can carry
/// several anonymised vessels under the one call sign `SHIP` (one
/// `ship:SHIP` id) at one time, and merging those would attribute one
/// vessel's values to another's position. Such reports stay separate, and
/// the store keeps the later one as before.
fn merge_same_observation(reports: &mut Vec<ObsReport>) {
    if reports.len() < 2 {
        return;
    }
    let mut index: HashMap<(String, DateTime<Utc>), usize> = HashMap::with_capacity(reports.len());
    let mut merged: Vec<ObsReport> = Vec::with_capacity(reports.len());
    for report in reports.drain(..) {
        match index.entry((report.station_id.clone(), report.time)) {
            Entry::Occupied(mut slot) => {
                let first = &mut merged[*slot.get()];
                if same_position(first, &report) {
                    first.name = first.name.take().or(report.name);
                    first.elevation = first.elevation.or(report.elevation);
                    first.elements.extend(report.elements);
                } else {
                    slot.insert(merged.len());
                    merged.push(report);
                }
            }
            Entry::Vacant(slot) => {
                slot.insert(merged.len());
                merged.push(report);
            }
        }
    }
    *reports = merged;
}

/// One fixed station's messages agree on its position to well within
/// 0.01°, even when one encodes the coarse `005002`/`006002` and the other
/// the high-accuracy `005001`/`006001`.
fn same_position(a: &ObsReport, b: &ObsReport) -> bool {
    const TOLERANCE_DEG: f64 = 0.01;
    (a.lat - b.lat).abs() <= TOLERANCE_DEG && (a.lon - b.lon).abs() <= TOLERANCE_DEG
}

fn find_magic(hay: &[u8]) -> Option<usize> {
    hay.windows(4).position(|w| w == b"BUFR")
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Missing => None,
        Value::Integer(i) => Some(*i as f64),
        Value::Decimal(m, s) => Some(*m as f64 * 10f64.powi(*s as i32)),
        Value::String(_) => None,
    }
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => {
            let t = s.trim().trim_matches('\0').trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        _ => None,
    }
}

impl Subset {
    fn take(&mut self, xy: XY, v: &Value) {
        match (xy.x, xy.y) {
            // ---- identity ------------------------------------------------
            (1, 125) => self.wigos[0] = num(v).map(|n| format!("{}", n as i64)),
            (1, 126) => self.wigos[1] = num(v).map(|n| format!("{}", n as i64)),
            (1, 127) => self.wigos[2] = num(v).map(|n| format!("{}", n as i64)),
            (1, 128) => self.wigos[3] = text(v),
            (1, 1) => self.block = self.block.or(num(v).map(|n| n as i64)),
            (1, 2) => self.station = self.station.or(num(v).map(|n| n as i64)),
            (1, 11) => self.callsign = self.callsign.take().or(text(v)),
            (1, 15) | (1, 19) => self.name = self.name.take().or(text(v)),
            // ---- position ------------------------------------------------
            (5, 1) | (5, 2) => self.lat = self.lat.or(num(v)),
            (6, 1) | (6, 2) => self.lon = self.lon.or(num(v)),
            (7, 30) | (7, 1) | (7, 7) => self.elevation = self.elevation.or(num(v)),
            // ---- time (first occurrence = nominal report time) ----------
            (4, 1..=6) => {
                let i = (xy.y - 1) as usize;
                if self.ymd[i].is_none() {
                    self.ymd[i] = num(v).map(|n| n as i64);
                }
            }
            // ---- period context ------------------------------------------
            (4, 23) => self.set_period(num(v).map(|d| d * 24.0)),
            (4, 24) => self.set_period(num(v)),
            (4, 25) => self.set_period(num(v).map(|m| m / 60.0)),
            (4, 26) => self.set_period(num(v).map(|s| s / 3600.0)),
            // ---- everything else: a data element -------------------------
            _ => {
                if let Some(value) = num(v) {
                    self.elements.push(Element {
                        xy,
                        value,
                        period_hours: self.period,
                    });
                }
            }
        }
    }

    /// Period semantics: a negative value opens a period ending at the
    /// nominal time (`-12` ⇒ the last 12 h); `0` is the "ends now" marker of
    /// a start/end pair and keeps the previous period; missing clears it.
    fn set_period(&mut self, hours: Option<f64>) {
        match hours {
            None => self.period = None,
            Some(h) if h < 0.0 => self.period = Some(-h),
            Some(h) if h > 0.0 => self.period = Some(h),
            // Exactly 0: the "ends now" marker of a start/end pair.
            Some(_) => {}
        }
    }

    fn finish(self) -> Result<ObsReport, SkipReason> {
        let station_id = match &self.wigos {
            [Some(a), Some(b), Some(c), Some(d)] => format!("{a}-{b}-{c}-{d}"),
            _ => match (self.block, self.station, &self.callsign) {
                (Some(b), Some(s), _) if (0..100).contains(&b) && (0..1000).contains(&s) => {
                    format!("0-20000-0-{b:02}{s:03}")
                }
                (_, _, Some(c)) => format!("ship:{c}"),
                _ => return Err(SkipReason::NoStationId),
            },
        };
        let (lat, lon) = match (self.lat, self.lon) {
            (Some(lat), Some(lon))
                if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) =>
            {
                (lat, lon)
            }
            _ => return Err(SkipReason::NoPosition),
        };
        let time = match self.ymd {
            [Some(y), Some(mo), Some(d), Some(h), mi, s] => Utc
                .with_ymd_and_hms(
                    y as i32,
                    mo as u32,
                    d as u32,
                    h as u32,
                    mi.unwrap_or(0) as u32,
                    s.unwrap_or(0) as u32,
                )
                .single()
                .ok_or(SkipReason::NoTime)?,
            _ => return Err(SkipReason::NoTime),
        };
        Ok(ObsReport {
            station_id,
            name: self.name,
            lat,
            lon,
            elevation: self.elevation,
            time,
            elements: self.elements,
        })
    }
}

impl ObsReport {
    /// First non-missing value of `xy` whose period matches `period_hours`
    /// (within a quarter hour), or any period when `None`.
    pub fn value(&self, xy: XY, period_hours: Option<f64>) -> Option<f64> {
        self.elements
            .iter()
            .find(|e| {
                e.xy == xy
                    && match period_hours {
                        None => true,
                        Some(p) => e
                            .period_hours
                            .map(|h| (h - p).abs() < 0.25)
                            .unwrap_or(false),
                    }
            })
            .map(|e| e.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xy_codes_round_trip() {
        let xy = xy_from_code("012101").unwrap();
        assert_eq!((xy.x, xy.y), (12, 101));
        assert_eq!(code_of(xy), "012101");
        assert!(xy_from_code("12101").is_none());
        assert!(xy_from_code("112101").is_none());
        assert!(xy_from_code("01210a").is_none());
    }

    #[test]
    fn period_context_rules() {
        let mut s = Subset::default();
        s.set_period(Some(-12.0));
        assert_eq!(s.period, Some(12.0));
        s.set_period(Some(0.0)); // end-of-period marker keeps 12
        assert_eq!(s.period, Some(12.0));
        s.set_period(None);
        assert_eq!(s.period, None);
        s.take(XY { x: 4, y: 25 }, &Value::Integer(-10));
        assert_eq!(s.period, Some(-(-10.0 / 60.0)));
    }

    #[test]
    fn station_id_fallbacks() {
        let ymd = [Some(2026), Some(9), Some(12), Some(8), Some(0), None];
        let s = Subset {
            lat: Some(1.0),
            lon: Some(2.0),
            ymd,
            ..Default::default()
        };
        assert_eq!(s.finish().unwrap_err(), SkipReason::NoStationId);

        let s = Subset {
            lat: Some(1.0),
            lon: Some(2.0),
            ymd,
            block: Some(2),
            station: Some(598),
            ..Default::default()
        };
        assert_eq!(s.finish().unwrap().station_id, "0-20000-0-02598");

        let s = Subset {
            lat: Some(1.0),
            lon: Some(2.0),
            ymd,
            callsign: Some("OXYH2".into()),
            ..Default::default()
        };
        assert_eq!(s.finish().unwrap().station_id, "ship:OXYH2");

        let s = Subset {
            lat: Some(95.0),
            lon: Some(2.0),
            ymd,
            callsign: Some("X".into()),
            ..Default::default()
        };
        assert_eq!(s.finish().unwrap_err(), SkipReason::NoPosition);

        let s = Subset {
            wigos: [
                Some("0".into()),
                Some("20000".into()),
                Some("0".into()),
                Some("02598".into()),
            ],
            lat: Some(1.0),
            lon: Some(2.0),
            ymd: [Some(2026), Some(9), Some(12), None, None, None],
            ..Default::default()
        };
        assert_eq!(s.finish().unwrap_err(), SkipReason::NoTime);
    }

    #[test]
    fn not_bufr_is_an_error() {
        let d = Decoder::new();
        assert!(matches!(d.decode(b"hello"), Err(DecodeError::NotBufr)));
        assert_eq!(DecodeError::NotBufr.kind(), FailureKind::NotBufr);
    }

    #[test]
    fn decoder_errors_map_to_failure_kinds() {
        let kind = |e: ds_bufr::Error| DecodeError::from(e).kind();
        let eof = std::io::Error::from(std::io::ErrorKind::UnexpectedEof);
        assert_eq!(kind(ds_bufr::Error::Io(eof)), FailureKind::Truncated);
        assert_eq!(
            kind(ds_bufr::Error::Table("x".into())),
            FailureKind::UnknownDescriptor
        );
        assert_eq!(
            kind(ds_bufr::Error::Invalid("x".into())),
            FailureKind::Invalid
        );
        assert_eq!(
            kind(ds_bufr::Error::NotSupported("x".into())),
            FailureKind::Unsupported
        );
        let labels: Vec<_> = FailureKind::ALL.iter().map(|k| k.label()).collect();
        assert_eq!(
            labels,
            [
                "not_bufr",
                "truncated",
                "unknown_descriptor",
                "invalid",
                "unsupported"
            ]
        );
        let reasons: Vec<_> = FailureKind::ALL.iter().map(|k| k.reason()).collect();
        assert_eq!(reasons, ["error", "error", "error", "error", "unsupported"]);
    }

    fn bits(t: &Tables, code: &str) -> Option<u16> {
        t.table_b.get(&xy_from_code(code).unwrap()).map(|e| e.bits)
    }

    #[test]
    fn older_master_versions_get_their_own_widths() {
        let d = Decoder::new();
        for (version, radiation) in [(0, 16), (13, 16), (14, 20), (31, 20), (45, 20)] {
            let t = d.tables_for(80, version, 0);
            assert_eq!(bits(t, "014029"), Some(radiation), "master {version}");
        }
        let v13 = d.tables_for(34, 13, 0);
        assert_eq!(bits(v13, "014002"), Some(12));
        assert_eq!(v13.table_b[&XY { x: 14, y: 2 }].reference_value, -2048);
        assert_eq!(bits(d.tables_for(34, 31, 0), "014002"), Some(17));
    }

    #[test]
    fn dwd_local_entries_follow_their_first_local_version() {
        let d = Decoder::new();
        for version in 0..=9u8 {
            let t = d.tables_for(78, 31, version);
            let has = |code: &str| bits(t, code).is_some();
            assert_eq!(has("020203"), (1..=8).contains(&version), "v{version}");
            assert_eq!(has("004215"), (2..=8).contains(&version), "v{version}");
            assert_eq!(has("020193"), (2..=8).contains(&version), "v{version}");
            assert_eq!(has("012195"), (6..=8).contains(&version), "v{version}");
            assert_eq!(has("020237"), (7..=8).contains(&version), "v{version}");
            assert_eq!(has("020216"), version == 8, "v{version}");
        }
        // Another centre never sees DWD's numbers.
        assert_eq!(bits(d.tables_for(98, 31, 8), "020193"), None);
        assert_eq!(bits(d.tables_for(78, 31, 8), "020193"), Some(7));
        // Versions that add nothing share one table set.
        assert!(Arc::ptr_eq(&d.dwd_tables[1], &d.dwd_tables[4]));
    }

    fn report(id: &str, hour: u32, elements: &[(&str, f64)]) -> ObsReport {
        ObsReport {
            station_id: id.into(),
            name: None,
            lat: 1.0,
            lon: 2.0,
            elevation: None,
            time: Utc.with_ymd_and_hms(2026, 10, 10, hour, 0, 0).unwrap(),
            elements: elements
                .iter()
                .map(|&(code, value)| Element {
                    xy: xy_from_code(code).unwrap(),
                    value,
                    period_hours: None,
                })
                .collect(),
        }
    }

    #[test]
    fn same_station_and_time_merge_with_the_first_message_winning() {
        let mut reports = vec![
            report("a", 3, &[("012101", 280.0)]),
            report("b", 3, &[("012101", 290.0)]),
            ObsReport {
                name: Some("A".into()),
                elevation: Some(5.0),
                ..report("a", 3, &[("012101", 1.0), ("020001", 9000.0)])
            },
            report("a", 4, &[("012101", 281.0)]),
        ];
        merge_same_observation(&mut reports);
        let keys: Vec<_> = reports
            .iter()
            .map(|r| (r.station_id.as_str(), r.time))
            .collect();
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0].0, "a");
        assert_eq!(keys[1].0, "b");
        let a = &reports[0];
        assert_eq!(a.name.as_deref(), Some("A"));
        assert_eq!(a.elevation, Some(5.0));
        assert_eq!(a.value(xy_from_code("012101").unwrap(), None), Some(280.0));
        assert_eq!(a.value(xy_from_code("020001").unwrap(), None), Some(9000.0));
        assert_eq!(
            reports[2].value(xy_from_code("012101").unwrap(), None),
            Some(281.0)
        );
    }

    #[test]
    fn same_id_and_time_at_another_position_does_not_merge() {
        // Anonymised vessels share the call sign `SHIP`: two of them in one
        // bulletin must not pool their values under the first's position.
        let mut reports = vec![
            report("ship:SHIP", 3, &[("012101", 280.0)]),
            ObsReport {
                lat: 40.0,
                lon: -30.0,
                ..report("ship:SHIP", 3, &[("012101", 290.0), ("020001", 9000.0)])
            },
            // Coarse vs high-accuracy encodings of one fixed station still merge.
            ObsReport {
                lat: 40.004,
                lon: -29.996,
                ..report("ship:SHIP", 3, &[("010051", 101_300.0)])
            },
        ];
        merge_same_observation(&mut reports);
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].elements.len(), 1);
        assert_eq!(
            reports[0].value(xy_from_code("020001").unwrap(), None),
            None
        );
        let second = &reports[1];
        assert_eq!((second.lat, second.lon), (40.0, -30.0));
        assert_eq!(
            second.value(xy_from_code("012101").unwrap(), None),
            Some(290.0)
        );
        assert_eq!(
            second.value(xy_from_code("010051").unwrap(), None),
            Some(101_300.0)
        );
    }
}
