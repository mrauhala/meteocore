//! Parser for ECMWF `.index` sidecar files (JSON-lines format).
//!
//! Each line maps a single GRIB message to its byte range within the
//! corresponding `.grib2` file, enabling efficient HTTP Range reads.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use serde::Deserialize;

use crate::catalog::MessageEntry;

/// Index file format.
///
/// `EcmwfJson` is the original ECMWF JSON-lines format (one JSON object per
/// line). `Wgrib2` is NOAA's wgrib2 colon-separated text format used by GFS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexFormat {
    EcmwfJson,
    Wgrib2,
}

impl IndexFormat {
    /// Parse from config string. Returns `EcmwfJson` for `None` (the default).
    pub fn from_config(s: Option<&str>) -> Option<Self> {
        match s {
            None | Some("ecmwf-json") => Some(Self::EcmwfJson),
            Some("wgrib2") => Some(Self::Wgrib2),
            _ => None,
        }
    }
}

/// Raw JSON structure of one line in an ECMWF `.index` file.
#[derive(Debug, Deserialize)]
struct IndexLine {
    #[allow(dead_code)]
    domain: Option<String>,
    date: String,
    time: String,
    #[allow(dead_code)]
    expver: Option<String>,
    step: String,
    levtype: String,
    levelist: Option<String>,
    param: String,
    #[serde(rename = "_offset")]
    offset: u64,
    #[serde(rename = "_length")]
    length: u64,
}

/// Parsed result from an index file.
pub struct IndexResult {
    /// Model reference time (run time), parsed from the index file.
    pub reference_time: DateTime<Utc>,
    /// Forecast step in hours.
    pub step: u32,
    /// Individual GRIB message entries.
    pub messages: Vec<MessageEntry>,
}

/// Parse the contents of an ECMWF `.index` file.
///
/// Returns `None` if the file is empty or unparseable.
pub fn parse_ecmwf_json(content: &str) -> Option<IndexResult> {
    let mut date = String::new();
    let mut time = String::new();
    let mut step: u32 = 0;
    let mut messages = Vec::new();
    let mut first = true;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let entry: IndexLine = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("Skipping unparseable index line: {e}");
                continue;
            }
        };

        if first {
            date.clone_from(&entry.date);
            time.clone_from(&entry.time);
            step = entry.step.parse().unwrap_or(0);
            first = false;
        }

        let level = entry.levelist.and_then(|l| l.parse::<u32>().ok());

        messages.push(MessageEntry {
            param: entry.param,
            levtype: entry.levtype,
            level,
            offset: entry.offset,
            length: Some(entry.length),
        });
    }

    if messages.is_empty() {
        return None;
    }

    let reference_time = parse_ecmwf_ref_time(&date, &time)?;

    Some(IndexResult {
        reference_time,
        step,
        messages,
    })
}

/// Parse an ECMWF-style reference time from separate `date` (YYYYMMDD) and
/// `time` (HHMM) strings.
fn parse_ecmwf_ref_time(date: &str, time: &str) -> Option<DateTime<Utc>> {
    let nd = NaiveDate::parse_from_str(date, "%Y%m%d").ok()?;
    let nt = NaiveTime::parse_from_str(time, "%H%M").ok()?;
    Some(nd.and_time(nt).and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ecmwf_json_index() {
        let content = r#"{"domain": "g", "date": "20260405", "time": "0000", "expver": "0001", "class": "od", "type": "fc", "stream": "oper", "step": "0", "levelist": "150", "levtype": "pl", "param": "q", "_offset": 0, "_length": 572555}
{"domain": "g", "date": "20260405", "time": "0000", "expver": "0001", "class": "od", "type": "fc", "stream": "oper", "step": "0", "levtype": "sfc", "param": "2d", "_offset": 572555, "_length": 688894}
{"domain": "g", "date": "20260405", "time": "0000", "expver": "0001", "class": "od", "type": "fc", "stream": "oper", "levtype": "sfc", "step": "0", "param": "ssrd", "_offset": 2557842, "_length": 224}"#;

        let result = parse_ecmwf_json(content).unwrap();
        assert_eq!(
            result.reference_time.to_rfc3339(),
            "2026-04-05T00:00:00+00:00"
        );
        assert_eq!(result.step, 0);
        assert_eq!(result.messages.len(), 3);

        // Pressure level message
        assert_eq!(result.messages[0].param, "q");
        assert_eq!(result.messages[0].levtype, "pl");
        assert_eq!(result.messages[0].level, Some(150));
        assert_eq!(result.messages[0].offset, 0);
        assert_eq!(result.messages[0].length, Some(572555));

        // Surface message (no levelist)
        assert_eq!(result.messages[1].param, "2d");
        assert_eq!(result.messages[1].levtype, "sfc");
        assert_eq!(result.messages[1].level, None);
    }

    #[test]
    fn parse_empty_index() {
        assert!(parse_ecmwf_json("").is_none());
        assert!(parse_ecmwf_json("   \n   \n").is_none());
    }

    #[test]
    fn index_format_from_config() {
        assert_eq!(IndexFormat::from_config(None), Some(IndexFormat::EcmwfJson));
        assert_eq!(
            IndexFormat::from_config(Some("ecmwf-json")),
            Some(IndexFormat::EcmwfJson)
        );
        assert_eq!(
            IndexFormat::from_config(Some("wgrib2")),
            Some(IndexFormat::Wgrib2)
        );
        assert_eq!(IndexFormat::from_config(Some("bogus")), None);
    }
}
