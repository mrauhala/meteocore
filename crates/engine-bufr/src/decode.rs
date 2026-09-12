//! BUFR → station reports. The **only** module that touches `tinybufr`, so
//! the decoder can be vendored or replaced in one place if a real feed hits
//! one of its gaps (compressed character strings, operators 203/204/207/22x).
//!
//! Extraction is template-agnostic: it walks the decoded element stream and
//! picks values by Table B descriptor, tracking the period context
//! (`004023`/`004024`/`004025`) that qualifies accumulations, extremes and
//! gusts. The v1 rule for repeated elements is **first non-missing occurrence
//! wins** per `(descriptor, period)` — in 307096 the 2 m air temperature comes
//! before the sensor-height replicas, and precipitation / extremes each carry
//! their own preceding period. Documented in `CLAUDE.md`.

use std::io::Cursor;
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use tinybufr::{DataEvent, DataReader, DataSpec, HeaderSections, Tables, Value, XY};

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

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("not a BUFR message")]
    NotBufr,
    #[error("bufr: {0}")]
    Bufr(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
}

impl From<tinybufr::Error> for DecodeError {
    fn from(e: tinybufr::Error) -> Self {
        match e {
            tinybufr::Error::NotSupported(m) => DecodeError::Unsupported(m),
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

/// Outcome of decoding one file: reports plus per-subset skips.
#[derive(Debug, Default)]
pub struct Decoded {
    pub reports: Vec<ObsReport>,
    pub skipped: Vec<SkipReason>,
    /// Number of BUFR messages found in the byte stream.
    pub messages: usize,
}

/// Holds the (expensive to build) WMO tables — construct once per engine.
pub struct Decoder {
    tables: Arc<Tables>,
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
        Decoder {
            tables: Arc::new(Tables::default()),
        }
    }

    /// Decode every BUFR message in `bytes` (files may concatenate several
    /// `BUFR…7777` messages; anything between messages is skipped).
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
            self.decode_message(&bytes[start..end], &mut out)?;
            out.messages += 1;
            pos = end.max(start + 4);
        }
        if out.messages == 0 {
            return Err(DecodeError::NotBufr);
        }
        Ok(out)
    }

    fn decode_message(&self, msg: &[u8], out: &mut Decoded) -> Result<(), DecodeError> {
        let mut reader = Cursor::new(msg);
        let header = HeaderSections::read(&mut reader)?;
        let spec = DataSpec::from_data_description(&header.data_description_section, &self.tables)?;
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
    }
}
