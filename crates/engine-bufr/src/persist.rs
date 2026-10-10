//! The WIS2 report store snapshot format (#1002).
//!
//! A WIS2 BUFR collection holds its station reports only in the in-memory
//! store, and nothing replays them after a restart: the broker does not
//! redeliver what it already delivered and there is no Global Cache
//! backfill, so a restart used to throw away the whole retention window
//! (about 270k reports on a global SYNOP feed, a day to refill). The engine
//! therefore snapshots the store into the server's
//! `ds_core::state::StateStore` under the key `<collection id>.bufr`
//! ([`KIND`]; the store, the atomic replace and the write policy are
//! `ds_core::state`'s) and restores it when the collection is built. This
//! module is only the bytes: one whole blob per snapshot.
//!
//! **Format: gzip-compressed JSON.** Measured on a synthetic global store
//! of 270k reports — 11,250 stations × 24 hourly SYNOP rows with realistic
//! value precision and missing-value patterns (`realistic_store_snapshot_size`,
//! release build): the snapshot is 20 MB of JSON, 7.7 MB (28 B/report)
//! after gzip's fast level, encoded in 0.25 s and decoded in 0.18 s. The
//! rows alone as plain JSON arrays of numbers and nulls would be 32 MB.
//! gzip's default level gets 5.5 MB but takes six times the CPU, which the
//! poll runtime would pay every five minutes. JSON is already in the tree
//! and stays inspectable (`gunzip -c | jq`); no compact serde format is in
//! the default build (CBOR and MessagePack come only with dev or optional
//! dependencies), and none would beat the row encoding below: a binary f32
//! is 4 bytes, while most stored values print in 2–6 characters and a
//! missing one costs a single `,`. flate2 is already compiled in through
//! ds-wis2.
//!
//! Layout: a header (`format`, `version`, `collection`, `written_at`, the
//! warm-up clock), the parameter `columns` the rows were written with, and
//! one entry per station, sorted by id (same state ⇒ same bytes). A
//! station's `times` are its row times in epoch seconds (BUFR times are
//! whole seconds), and each row is ONE string of comma-separated values in
//! column order: a missing value is empty and trailing missing values are
//! dropped, so `"8.2,-1.5,,1013.2"`. Values are written with Rust's
//! shortest round-trip `f32` formatting and read back with `f32::from_str`,
//! which is correctly rounded: every stored value reads back bit for bit,
//! without going through `f64` (pinned by `row_values_round_trip_bit_exactly`).
//! Station coordinates go through serde_json's `float_roundtrip`.
//!
//! The columns make a snapshot independent of the parameter table it was
//! written under: restore maps each snapshot column onto the current
//! table's column with the SAME definition (name, descriptors, source
//! unit, stored unit, period); a column the config dropped or redefined is
//! dropped, and a new one is missing in the restored rows. A snapshot
//! whose `format`, `version` or `collection` does not match, or that is
//! malformed in any way, is rejected whole: the engine logs it and starts
//! cold. `written_at` is when the blob was encoded: the engine rewrites an
//! unchanged store now and then and flushes at shutdown, so it also says
//! how long the server was down when it is restored.
//!
//! Not in the snapshot: the `rel=deletion` index (which `data_id` produced
//! which row). It is bounded to the last ~200k `data_id`s (about 5 h of a
//! global feed), whose ~100-character strings alone are about as large as
//! the store's whole JSON, and observation feeds withdraw data rarely; a deletion of a `data_id`
//! received before the restart is a no-op and its rows age out with
//! `retention`.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::io::{BufWriter, Read};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::health::WarmupCause;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize, Serializer};

use crate::decode::code_of;
use crate::params::{ParamDef, ParameterTable};
use crate::store::{ObsStore, StationInfo, StationSeries};
use crate::wis2::WarmupClock;

/// The store key's kind: `<collection id>.bufr` (`ds_core::state::collection_key`).
pub(crate) const KIND: &str = "bufr";
const FORMAT: &str = "meteocore/bufr-wis2-store";
/// Bump on any non-additive change to this file's types.
pub(crate) const VERSION: u32 = 1;
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
/// Refuse to inflate a snapshot past this (a global store is ~20 MB): a
/// corrupt or foreign file must not exhaust memory before it is rejected.
const MAX_INFLATED_BYTES: u64 = 1 << 30;
/// JSON is written through this buffer into the compressor.
const ENCODE_BUFFER_BYTES: usize = 64 * 1024;

/// Rows of one station, keyed by report time (the store's own shape).
pub(crate) type Rows = BTreeMap<DateTime<Utc>, Box<[f32]>>;

/// One store column as written: everything that decides what its values
/// mean. `label`/`observed_property` are presentation and may change freely.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ColumnRepr {
    name: String,
    descriptors: Vec<String>,
    source_unit: String,
    unit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    period_hours: Option<f64>,
}

impl From<&ParamDef> for ColumnRepr {
    fn from(p: &ParamDef) -> Self {
        ColumnRepr {
            name: p.name.clone(),
            descriptors: p.descriptors.iter().map(|&xy| code_of(xy)).collect(),
            source_unit: p.source_unit.clone(),
            unit: p.unit.clone(),
            period_hours: p.period_hours,
        }
    }
}

/// [`WarmupCause`], kept apart so the health type is not the file format
/// (the same spelling as engine-cap's snapshot).
#[derive(Serialize, Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
enum CauseRepr {
    #[default]
    ColdStart,
    LongOutage,
}

impl From<WarmupCause> for CauseRepr {
    fn from(c: WarmupCause) -> Self {
        match c {
            WarmupCause::ColdStart => CauseRepr::ColdStart,
            WarmupCause::LongOutage => CauseRepr::LongOutage,
        }
    }
}

impl From<CauseRepr> for WarmupCause {
    fn from(c: CauseRepr) -> Self {
        match c {
            CauseRepr::ColdStart => WarmupCause::ColdStart,
            CauseRepr::LongOutage => WarmupCause::LongOutage,
        }
    }
}

// ---- writing ---------------------------------------------------------------

#[derive(Serialize)]
struct SnapshotOut<'a> {
    format: &'static str,
    version: u32,
    collection: &'a str,
    written_at: DateTime<Utc>,
    filling_since: Option<DateTime<Utc>>,
    warmup_cause: CauseRepr,
    columns: Vec<ColumnRepr>,
    stations: Vec<StationOut<'a>>,
}

#[derive(Serialize)]
struct StationOut<'a> {
    id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    lat: f64,
    lon: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    elevation: Option<f64>,
    last_seen: DateTime<Utc>,
    times: TimesOut<'a>,
    rows: RowsOut<'a>,
}

/// A station's row times as epoch seconds, straight from the store.
struct TimesOut<'a>(&'a Rows);

impl Serialize for TimesOut<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.0.keys().map(|t| t.timestamp()))
    }
}

/// A station's rows, each formatted on the fly into one reused buffer.
struct RowsOut<'a>(&'a Rows);

impl Serialize for RowsOut<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        let mut buf = String::new();
        for row in self.0.values() {
            buf.clear();
            write_row(&mut buf, row);
            seq.serialize_element(buf.as_str())?;
        }
        seq.end()
    }
}

/// `8.2,-1.5,,1013.2`: values in column order, a missing (`NaN`) one
/// empty, trailing missing ones dropped. `{}` on an `f32` is its shortest
/// round-trip form (`8.2`, `-0`, `20000`, `inf`).
fn write_row(buf: &mut String, row: &[f32]) {
    let len = row.iter().rposition(|v| !v.is_nan()).map_or(0, |i| i + 1);
    for (i, v) in row[..len].iter().enumerate() {
        if i > 0 {
            buf.push(',');
        }
        if !v.is_nan() {
            let _ = write!(buf, "{v}");
        }
    }
}

/// The store as one snapshot blob. Runs under the store's read lock: it
/// reads the rows in place (no copy of the store) and formats each row
/// into one reused buffer.
pub(crate) fn encode(
    collection_id: &str,
    store: &ObsStore,
    table: &ParameterTable,
    clock: WarmupClock,
    written_at: DateTime<Utc>,
) -> Result<Vec<u8>, String> {
    let mut series: Vec<&StationSeries> = store.stations().collect();
    series.sort_unstable_by(|a, b| a.info.id.cmp(&b.info.id));
    let snapshot = SnapshotOut {
        format: FORMAT,
        version: VERSION,
        collection: collection_id,
        written_at,
        filling_since: clock.filling_since,
        warmup_cause: clock.cause.into(),
        columns: table.params.iter().map(ColumnRepr::from).collect(),
        stations: series
            .into_iter()
            .map(|s| StationOut {
                id: &s.info.id,
                name: s.info.name.as_deref(),
                lat: s.info.lat,
                lon: s.info.lon,
                elevation: s.info.elevation.filter(|e| e.is_finite()),
                last_seen: s.info.last_seen,
                times: TimesOut(&s.rows),
                rows: RowsOut(&s.rows),
            })
            .collect(),
    };
    // Fast level: 85 % of the default level's savings for a sixth of its
    // CPU, which the poll runtime pays every five minutes. The buffer
    // matters: serde_json writes fragments of a few bytes, and deflating
    // each fragment made the encode four times slower.
    let mut out = BufWriter::with_capacity(
        ENCODE_BUFFER_BYTES,
        GzEncoder::new(Vec::new(), Compression::fast()),
    );
    serde_json::to_writer(&mut out, &snapshot).map_err(|e| e.to_string())?;
    out.into_inner()
        .map_err(|e| e.error().to_string())?
        .finish()
        .map_err(|e| e.to_string())
}

// ---- reading ---------------------------------------------------------------

/// Read first, so a snapshot from another version or collection is
/// reported as that rather than as whatever field fails to parse.
#[derive(Deserialize)]
struct Header {
    format: String,
    version: u32,
    collection: String,
}

#[derive(Deserialize)]
struct SnapshotIn<'a> {
    written_at: DateTime<Utc>,
    filling_since: Option<DateTime<Utc>>,
    #[serde(default)]
    warmup_cause: CauseRepr,
    columns: Vec<ColumnRepr>,
    #[serde(borrow)]
    stations: Vec<StationIn<'a>>,
}

#[derive(Deserialize)]
struct StationIn<'a> {
    id: String,
    #[serde(default)]
    name: Option<String>,
    lat: f64,
    lon: f64,
    #[serde(default)]
    elevation: Option<f64>,
    last_seen: DateTime<Utc>,
    times: Vec<i64>,
    #[serde(borrow)]
    rows: Vec<RowText<'a>>,
}

/// One row string, borrowed from the inflated buffer (an escape, which the
/// writer never produces, falls back to an owned copy).
#[derive(Deserialize)]
struct RowText<'a>(#[serde(borrow)] Cow<'a, str>);

/// A decoded snapshot: stations with their rows already in the CURRENT
/// parameter table's column layout, and the warm-up clock.
pub(crate) struct Decoded {
    pub(crate) written_at: DateTime<Utc>,
    pub(crate) clock: WarmupClock,
    pub(crate) stations: Vec<(StationInfo, Rows)>,
    /// Snapshot columns the current table has no identical column for:
    /// their values were dropped.
    pub(crate) columns_dropped: Vec<String>,
}

/// Inflate, parse and check a snapshot written for `collection_id`, and
/// map its rows onto `table`. Any problem rejects the whole snapshot: a
/// partially restored store would claim a history it does not have.
pub(crate) fn decode(
    bytes: &[u8],
    collection_id: &str,
    table: &ParameterTable,
) -> Result<Decoded, String> {
    if !bytes.starts_with(&GZIP_MAGIC) {
        return Err("not a snapshot: not gzip-compressed".into());
    }
    let mut json = Vec::new();
    GzDecoder::new(bytes)
        .take(MAX_INFLATED_BYTES + 1)
        .read_to_end(&mut json)
        .map_err(|e| format!("not a snapshot: {e}"))?;
    if json.len() as u64 > MAX_INFLATED_BYTES {
        return Err(format!(
            "not a snapshot: inflates past {MAX_INFLATED_BYTES} bytes"
        ));
    }
    let header: Header =
        serde_json::from_slice(&json).map_err(|e| format!("not a snapshot: {e}"))?;
    if header.format != FORMAT {
        return Err(format!("unknown format '{}'", header.format));
    }
    if header.version != VERSION {
        return Err(format!(
            "snapshot version {} (this build reads {VERSION})",
            header.version
        ));
    }
    if header.collection != collection_id {
        return Err(format!(
            "snapshot of collection '{}', not '{collection_id}'",
            header.collection
        ));
    }
    let snapshot: SnapshotIn = serde_json::from_slice(&json).map_err(|e| e.to_string())?;

    // Snapshot column → current column with the same definition.
    let mut names = HashSet::new();
    let mut columns_dropped = Vec::new();
    let mut map = Vec::with_capacity(snapshot.columns.len());
    for col in &snapshot.columns {
        if !names.insert(col.name.as_str()) {
            return Err(format!("duplicate column '{}'", col.name));
        }
        let current = table
            .index_of(&col.name)
            .filter(|&j| ColumnRepr::from(&table.params[j]) == *col);
        if current.is_none() {
            columns_dropped.push(col.name.clone());
        }
        map.push(current);
    }

    let mut ids = HashSet::with_capacity(snapshot.stations.len());
    let mut stations = Vec::with_capacity(snapshot.stations.len());
    for s in snapshot.stations {
        if !ids.insert(s.id.clone()) {
            return Err(format!("duplicate station '{}'", s.id));
        }
        if !(-90.0..=90.0).contains(&s.lat) || !(-180.0..=180.0).contains(&s.lon) {
            return Err(format!("station '{}': position out of range", s.id));
        }
        if s.times.len() != s.rows.len() {
            return Err(format!(
                "station '{}': {} times for {} rows",
                s.id,
                s.times.len(),
                s.rows.len()
            ));
        }
        let mut rows = Rows::new();
        for (&t, text) in s.times.iter().zip(&s.rows) {
            let time = DateTime::from_timestamp(t, 0)
                .ok_or_else(|| format!("station '{}': bad time {t}", s.id))?;
            if rows.last_key_value().is_some_and(|(&last, _)| last >= time) {
                return Err(format!("station '{}': times not increasing", s.id));
            }
            let row = parse_row(&text.0, &map, table.len())
                .map_err(|e| format!("station '{}' at {time}: {e}", s.id))?;
            rows.insert(time, row);
        }
        let (Some((&first, _)), Some((&last, _))) = (rows.first_key_value(), rows.last_key_value())
        else {
            return Err(format!("station '{}': no rows", s.id));
        };
        let info = StationInfo {
            id: Arc::from(s.id),
            name: s.name,
            lat: s.lat,
            lon: s.lon,
            elevation: s.elevation,
            first_report: first,
            last_report: last,
            last_seen: s.last_seen,
        };
        stations.push((info, rows));
    }
    Ok(Decoded {
        written_at: snapshot.written_at,
        clock: WarmupClock {
            filling_since: snapshot.filling_since,
            cause: snapshot.warmup_cause.into(),
        },
        stations,
        columns_dropped,
    })
}

/// One row string → a dense row of `width` columns through `map`
/// (snapshot column → current column, `None` = dropped).
fn parse_row(text: &str, map: &[Option<usize>], width: usize) -> Result<Box<[f32]>, String> {
    let mut row = vec![f32::NAN; width].into_boxed_slice();
    if text.is_empty() {
        return Ok(row);
    }
    for (i, field) in text.split(',').enumerate() {
        let Some(&target) = map.get(i) else {
            return Err(format!("more than {} values", map.len()));
        };
        if field.is_empty() {
            continue;
        }
        let v: f32 = field
            .parse()
            .map_err(|_| format!("value '{field}' is not a number"))?;
        if let Some(j) = target {
            row[j] = v;
        }
    }
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{xy_from_code, Element, ObsReport};
    use chrono::{Duration, TimeZone};
    use ds_core::config::BufrParameterConfig;
    use std::io::Write;

    fn t(day: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, day, h, 0, 0).unwrap()
    }

    fn el(code: &str, value: f64, period_hours: Option<f64>) -> Element {
        Element {
            xy: xy_from_code(code).unwrap(),
            value,
            period_hours,
        }
    }

    fn report(id: &str, time: DateTime<Utc>, elements: Vec<Element>) -> ObsReport {
        ObsReport {
            station_id: id.into(),
            name: Some(format!("Station {id}")),
            lat: 57.6727,
            lon: 18.345_678_901_234_567,
            elevation: Some(47.0),
            time,
            elements,
        }
    }

    /// Two stations (one a ship without name or elevation), hourly rows
    /// with full, partial and empty rows, a correction, and values that
    /// exercise the formatter (negative, `-0`, large, long fractions).
    fn populated(table: &ParameterTable) -> ObsStore {
        let mut s = ObsStore::new(Duration::hours(24), 100);
        let now = t(12, 12);
        for h in 0..12 {
            s.ingest(
                &report(
                    "0-20000-0-02598",
                    t(12, h),
                    vec![
                        el("012101", 281.35 - f64::from(h) * 0.37, None),
                        el("012103", 273.15, None),
                        el("010004", 101_325.0 + f64::from(h) * 10.0, None),
                        el("013011", 0.2, Some(1.0)),
                        el("020001", 20_000.0, None),
                    ],
                ),
                table,
                now,
            );
        }
        s.ingest(&report("0-20000-0-02598", t(12, 3), vec![]), table, now);
        let mut ship = report("ship:SHIP", t(12, 6), vec![el("012101", 300.123_456, None)]);
        ship.name = None;
        ship.elevation = None;
        ship.lat = -33.9;
        ship.lon = -179.999_9;
        s.ingest(&ship, table, now);
        s
    }

    fn assert_same_store(a: &ObsStore, b: &ObsStore) {
        assert_eq!(a.station_count(), b.station_count());
        assert_eq!(a.row_count(), b.row_count());
        for sa in a.stations() {
            let sb = b.get(&sa.info.id).expect("station restored");
            assert_eq!(sa.info, sb.info);
            assert_eq!(sa.info.lat.to_bits(), sb.info.lat.to_bits());
            assert_eq!(sa.info.lon.to_bits(), sb.info.lon.to_bits());
            assert_eq!(
                sa.rows.keys().collect::<Vec<_>>(),
                sb.rows.keys().collect::<Vec<_>>()
            );
            for (ra, rb) in sa.rows.values().zip(sb.rows.values()) {
                let bits = |r: &[f32]| {
                    r.iter()
                        .map(|v| (!v.is_nan()).then(|| v.to_bits()))
                        .collect::<Vec<_>>()
                };
                assert_eq!(bits(ra), bits(rb));
            }
        }
    }

    fn restore_into(decoded: Decoded, retention: Duration) -> ObsStore {
        let mut s = ObsStore::new(retention, 100);
        for (info, rows) in decoded.stations {
            s.restore_station(info, rows);
        }
        s
    }

    fn clock() -> WarmupClock {
        WarmupClock {
            filling_since: Some(t(11, 7)),
            cause: WarmupCause::LongOutage,
        }
    }

    fn inflate(bytes: &[u8]) -> serde_json::Value {
        let mut json = Vec::new();
        GzDecoder::new(bytes).read_to_end(&mut json).unwrap();
        serde_json::from_slice(&json).unwrap()
    }

    fn deflate(v: &serde_json::Value) -> Vec<u8> {
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        serde_json::to_writer(&mut gz, v).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn round_trip_restores_the_store_exactly() {
        let table = ParameterTable::build(true, &[]);
        let store = populated(&table);
        assert_eq!((store.station_count(), store.row_count()), (2, 13));
        let bytes = encode("obs", &store, &table, clock(), t(12, 12)).unwrap();

        let decoded = decode(&bytes, "obs", &table).unwrap();
        assert_eq!(decoded.written_at, t(12, 12));
        assert_eq!(decoded.clock.filling_since, Some(t(11, 7)));
        assert_eq!(decoded.clock.cause, WarmupCause::LongOutage);
        assert!(decoded.columns_dropped.is_empty());
        let restored = restore_into(decoded, Duration::hours(24));
        assert_same_store(&store, &restored);
        // Same state ⇒ same bytes (stations sorted, gzip without mtime).
        assert_eq!(
            encode("obs", &restored, &table, clock(), t(12, 12)).unwrap(),
            bytes
        );

        // The row encoding: shortest values, empty for missing, no
        // trailing commas; the empty report is an empty row.
        let v = inflate(&bytes);
        assert_eq!(v["format"], FORMAT);
        assert_eq!(v["warmup_cause"], "long_outage");
        assert_eq!(v["columns"][0]["name"], "air_temperature");
        assert_eq!(v["columns"][0]["descriptors"][0], "012101");
        assert_eq!(v["columns"][0]["unit"], "°C");
        assert_eq!(v["columns"][9]["period_hours"], 1.0);
        let station = &v["stations"][0];
        assert_eq!(station["id"], "0-20000-0-02598");
        assert_eq!(station["times"][0], t(12, 0).timestamp());
        assert_eq!(station["rows"][0], "8.2,0,,1013.25,,,,,,0.2,,,,,,,,,20000");
        assert_eq!(station["rows"][3], "");
        let ship = &v["stations"][1];
        assert!(ship.get("name").is_none() && ship.get("elevation").is_none());
    }

    /// Every finite `f32` the store can hold reads back bit for bit — a
    /// sweep over bit patterns (all exponents) plus the edge values —
    /// through the text form, never through `f64`.
    #[test]
    fn row_values_round_trip_bit_exactly() {
        let mut x: u32 = 0x9E37_79B9;
        let mut values: Vec<f32> = (0..200_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                f32::from_bits(x)
            })
            .filter(|v| !v.is_nan())
            .collect();
        values.extend([
            0.0,
            -0.0,
            f32::MIN_POSITIVE,
            -f32::MIN_POSITIVE,
            f32::from_bits(1),
            f32::MAX,
            f32::MIN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            16.97,
            (290.12_f64 - 273.15) as f32,
        ]);
        let width = values.len();
        let map: Vec<Option<usize>> = (0..width).map(Some).collect();
        let mut buf = String::new();
        write_row(&mut buf, &values);
        let back = parse_row(&buf, &map, width).unwrap();
        let mismatched = values
            .iter()
            .zip(back.iter())
            .filter(|(a, b)| a.to_bits() != b.to_bits())
            .count();
        assert_eq!(mismatched, 0);
    }

    #[test]
    fn restore_maps_columns_by_definition_under_a_changed_config() {
        let table = ParameterTable::build(true, &[]);
        let bytes = encode("obs", &populated(&table), &table, clock(), t(12, 12)).unwrap();
        // Since the snapshot: built-ins off; air_temperature redefined
        // (another descriptor), visibility kept as is, a new column added.
        let param = |name: &str, code: &str, unit: &str| BufrParameterConfig {
            name: name.into(),
            descriptors: vec![code.into()],
            unit: unit.into(),
            label: None,
            observed_property: None,
            period_hours: None,
        };
        let changed = ParameterTable::build(
            false,
            &[
                param("new_column", "012102", "K"),
                param("visibility", "020001", "m"),
                param("air_temperature", "012004", "K"),
            ],
        );
        let decoded = decode(&bytes, "obs", &changed).unwrap();
        assert_eq!(decoded.columns_dropped.len(), 21);
        assert!(decoded
            .columns_dropped
            .contains(&"air_temperature".to_string()));
        assert!(!decoded.columns_dropped.contains(&"visibility".to_string()));
        let restored = restore_into(decoded, Duration::hours(24));
        let row = &restored.get("0-20000-0-02598").unwrap().rows[&t(12, 0)];
        assert_eq!(row.len(), 3);
        assert!(row[0].is_nan(), "new column: missing in restored rows");
        assert_eq!(row[1], 20_000.0, "visibility: same definition, kept");
        assert!(row[2].is_nan(), "air_temperature: redefined, dropped");
    }

    #[test]
    fn malformed_snapshots_are_rejected_whole() {
        let table = ParameterTable::build(true, &[]);
        let bytes = encode("obs", &populated(&table), &table, clock(), t(12, 12)).unwrap();
        let err = |b: &[u8]| decode(b, "obs", &table).err().expect("must be rejected");

        assert!(err(b"").starts_with("not a snapshot"));
        assert!(err(b"{\"format\":1}").contains("not gzip"));
        assert!(err(&bytes[..bytes.len() / 2]).starts_with("not a snapshot"));
        // A flipped byte in the deflate stream or the CRC trailer.
        let mut flipped = bytes.clone();
        let n = flipped.len();
        flipped[n - 6] ^= 0xFF;
        assert!(err(&flipped).starts_with("not a snapshot"));
        let v = inflate(&bytes);
        assert!(err(&deflate(&serde_json::json!([1, 2, 3]))).starts_with("not a snapshot"));
        assert_eq!(
            decode(&bytes, "other", &table).err().unwrap(),
            "snapshot of collection 'obs', not 'other'"
        );
        let edit = |f: &dyn Fn(&mut serde_json::Value)| {
            let mut bad = v.clone();
            f(&mut bad);
            err(&deflate(&bad))
        };
        assert!(edit(&|b| b["version"] = 99.into()).contains("snapshot version 99"));
        assert!(
            edit(&|b| b["format"] = "meteocore/cap-wis2-accumulator".into())
                .contains("unknown format")
        );
        assert!(
            edit(&|b| b["stations"][0]["rows"][1] = "8.2,x".into()).contains("'x' is not a number")
        );
        assert!(
            edit(&|b| b["stations"][0]["rows"][1] = ",".repeat(22).into())
                .contains("more than 22 values")
        );
        assert!(
            edit(&|b| b["stations"][0]["times"][1] = b["stations"][0]["times"][0].clone())
                .contains("times not increasing")
        );
        assert!(edit(&|b| {
            b["stations"][0]["times"].as_array_mut().unwrap().pop();
        })
        .contains("11 times for 12 rows"));
        assert!(edit(&|b| b["stations"][0]["times"][0] = i64::MAX.into()).contains("bad time"));
        assert!(edit(&|b| {
            b["stations"][0]["times"] = serde_json::json!([]);
            b["stations"][0]["rows"] = serde_json::json!([]);
        })
        .contains("no rows"));
        assert!(
            edit(&|b| b["stations"][1]["id"] = b["stations"][0]["id"].clone())
                .contains("duplicate station")
        );
        assert!(edit(&|b| b["stations"][0]["lat"] = 91.0.into()).contains("out of range"));
        assert!(
            edit(&|b| b["columns"][1]["name"] = "air_temperature".into())
                .contains("duplicate column")
        );
        assert!(!edit(&|b| b["written_at"] = "yesterday".into()).is_empty());
    }

    /// A synthetic global SYNOP store — 11,250 stations × 24 hourly rows =
    /// 270,000 reports, the size a live global feed holds at the default
    /// PT24H retention — with realistic precision and missing-value
    /// patterns. Prints the snapshot size and timings; run it with
    /// `cargo test --release -p engine-bufr realistic_store_snapshot_size
    /// -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn realistic_store_snapshot_size() {
        let table = ParameterTable::build(true, &[]);
        let store = synthetic_store(&table, 11_250, 24);
        assert_eq!(store.row_count(), 270_000);
        let start = std::time::Instant::now();
        let bytes = encode("obs", &store, &table, clock(), t(12, 12)).unwrap();
        let encode_time = start.elapsed();
        let mut json = Vec::new();
        GzDecoder::new(&bytes[..]).read_to_end(&mut json).unwrap();
        let start = std::time::Instant::now();
        let decoded = decode(&bytes, "obs", &table).unwrap();
        let decode_time = start.elapsed();
        assert_eq!(decoded.stations.len(), 11_250);

        // The alternatives the format was chosen over, for the record:
        // plain JSON rows (numbers and nulls) and the default gzip level.
        let plain: Vec<Vec<Option<f32>>> = store
            .stations()
            .flat_map(|s| s.rows.values())
            .map(|r| r.iter().map(|v| (!v.is_nan()).then_some(*v)).collect())
            .collect();
        let plain = serde_json::to_vec(&plain).unwrap();
        let start = std::time::Instant::now();
        let mut best = GzEncoder::new(Vec::new(), Compression::default());
        best.write_all(&json).unwrap();
        let best = best.finish().unwrap();
        let best_time = start.elapsed();
        let start = std::time::Instant::now();
        let mut fast = GzEncoder::new(Vec::new(), Compression::fast());
        fast.write_all(&json).unwrap();
        let fast = fast.finish().unwrap();
        println!(
            "gzip fast alone: {} bytes in {:?}",
            fast.len(),
            start.elapsed()
        );
        let start = std::time::Instant::now();
        let mut buf = String::new();
        let mut n = 0;
        for s in store.stations() {
            for r in s.rows.values() {
                buf.clear();
                write_row(&mut buf, r);
                n += buf.len();
            }
        }
        println!("row formatting alone: {n} bytes in {:?}", start.elapsed());
        println!(
            "270k reports: snapshot {} bytes ({:.1} B/report), JSON {} bytes; encode {:?}, \
             decode {:?}; plain JSON rows {} bytes; gzip default level {} bytes in {:?}",
            bytes.len(),
            bytes.len() as f64 / 270_000.0,
            json.len(),
            encode_time,
            decode_time,
            plain.len(),
            best.len(),
            best_time,
        );
    }

    /// Pins the encoding's compactness on a smaller synthetic store, so a
    /// format change that bloats it (plain JSON rows, no compression)
    /// fails here rather than on a deployment's disk.
    #[test]
    fn snapshot_stays_compact() {
        let table = ParameterTable::build(true, &[]);
        let store = synthetic_store(&table, 500, 24);
        let bytes = encode("obs", &store, &table, clock(), t(12, 12)).unwrap();
        // 28.4 today, as at 270k reports; uncompressed it would be ~75.
        let per_report = bytes.len() as f64 / 12_000.0;
        assert!(per_report < 35.0, "{per_report:.1} bytes per report");
    }

    /// `stations` × `hours` hourly SYNOP rows ending at 12Z on the 12th.
    fn synthetic_store(table: &ParameterTable, stations: u32, hours: u32) -> ObsStore {
        use chrono::Timelike;
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        let mut rand = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut store = ObsStore::new(Duration::hours(48), 100_000);
        let now = t(12, 12);
        for n in 0..stations {
            let lat = ((rand() * 140.0 - 60.0) * 1e4).round() / 1e4;
            let lon = ((rand() * 360.0 - 180.0) * 1e4).round() / 1e4;
            let elevation = (rand() * 2_000.0).round();
            let base_t = 250.0 + rand() * 50.0;
            let base_p = 95_000.0 + rand() * 8_000.0;
            for h in 0..hours {
                let time = now - Duration::hours(i64::from(h));
                let temp = ((base_t + rand() * 6.0) * 100.0).round() / 100.0;
                let p = ((base_p + rand() * 300.0) / 10.0).round() * 10.0;
                // (share of reports carrying it, descriptor, value, period)
                let mut candidates = vec![
                    (1.0, "012101", temp, None),
                    (0.9, "012103", temp - (rand() * 800.0).round() / 100.0, None),
                    (0.8, "013003", (rand() * 70.0 + 30.0).round(), None),
                    (0.85, "010004", p, None),
                    (0.85, "010051", p + 1_200.0, None),
                    (0.3, "010061", ((rand() - 0.5) * 60.0).round() * 10.0, None),
                    (0.95, "011001", (rand() * 36.0).round() * 10.0, None),
                    (0.95, "011002", (rand() * 150.0).round() / 10.0, None),
                    (0.3, "011041", (rand() * 250.0).round() / 10.0, None),
                    (0.2, "013011", (rand() * 50.0).round() / 10.0, Some(1.0)),
                    (0.1, "013011", (rand() * 50.0).round() / 10.0, Some(3.0)),
                    (
                        0.7,
                        "020001",
                        [50e3, 20e3, 10e3, 4.5e3][h as usize % 4],
                        None,
                    ),
                    (0.6, "020010", f64::from(h % 9) * 12.5, None),
                    (0.4, "020003", (rand() * 100.0).floor(), None),
                    (0.05, "013013", (rand() * 100.0).round() / 100.0, None),
                ];
                if time.hour().is_multiple_of(6) {
                    for (period, share) in [(6.0, 0.8), (12.0, 0.5), (24.0, 0.2)] {
                        let v = (rand() * 200.0).round() / 10.0;
                        candidates.push((share, "013011", v, Some(period)));
                    }
                    for (code, period) in [("012111", 12.0), ("012112", 12.0), ("012111", 24.0)] {
                        let v = temp + (rand() * 500.0).round() / 100.0;
                        candidates.push((0.3, code, v, Some(period)));
                    }
                }
                let elements = candidates
                    .into_iter()
                    .filter(|&(share, ..)| rand() < share)
                    .map(|(_, code, v, period)| el(code, v, period))
                    .collect();
                let r = ObsReport {
                    station_id: format!("0-20000-0-{:05}", 10_000 + n),
                    name: Some(format!("STATION {n}")),
                    lat,
                    lon,
                    elevation: Some(elevation),
                    time,
                    elements,
                };
                store.ingest(&r, table, now);
            }
        }
        store
    }
}
