use chrono::{DateTime, FixedOffset, Utc};
use std::collections::{BTreeMap, HashMap};

use ds_core::error::DataServerError;

#[derive(Debug, Clone)]
pub struct CsvRow {
    pub location: String,
    pub latitude: f64,
    pub longitude: f64,
    pub time: DateTime<Utc>,
    pub values: HashMap<String, Option<f64>>,
}

#[derive(Debug)]
pub struct CsvDataStore {
    pub rows: Vec<CsvRow>,
    pub location_index: HashMap<String, Vec<usize>>,
    pub time_index: HashMap<String, BTreeMap<DateTime<Utc>, Vec<usize>>>,
    pub parameter_names: Vec<String>,
    pub parameter_units: HashMap<String, String>,
}

impl CsvDataStore {
    pub fn load(path: &str) -> Result<Self, DataServerError> {
        let mut reader = csv::Reader::from_path(path)
            .map_err(|e| DataServerError::Engine(format!("Failed to open {path}: {e}")))?;

        let headers: Vec<String> = reader
            .headers()
            .map_err(|e| DataServerError::Engine(format!("Failed to read headers: {e}")))?
            .iter()
            .map(|h| h.to_string())
            .collect();

        // Fixed columns: location, latitude, longitude, time
        // Everything else is a parameter
        let param_start = 4;
        if headers.len() < param_start {
            return Err(DataServerError::Engine(format!(
                "CSV must have at least {} columns (location, latitude, longitude, time), found {}",
                param_start,
                headers.len()
            )));
        }
        let parameter_names: Vec<String> = headers[param_start..].to_vec();

        let parameter_units: HashMap<String, String> = parameter_names
            .iter()
            .map(|name| {
                let unit = match name.as_str() {
                    "temperature" => "°C",
                    "humidity" => "%",
                    "wind_speed" => "m/s",
                    "pressure" => "hPa",
                    "precipitation" => "mm",
                    _ => "",
                };
                (name.clone(), unit.to_string())
            })
            .collect();

        let mut rows = Vec::new();
        let mut location_index: HashMap<String, Vec<usize>> = HashMap::new();
        let mut time_index: HashMap<String, BTreeMap<DateTime<Utc>, Vec<usize>>> = HashMap::new();
        let mut offsets = OffsetTally::default();

        for result in reader.records() {
            let record =
                result.map_err(|e| DataServerError::Engine(format!("Failed to read row: {e}")))?;

            let location = record[0].to_string();
            let latitude: f64 = record[1]
                .parse()
                .map_err(|e| DataServerError::Engine(format!("Invalid latitude: {e}")))?;
            let longitude: f64 = record[2]
                .parse()
                .map_err(|e| DataServerError::Engine(format!("Invalid longitude: {e}")))?;
            let time = parse_time(&record[3])?;
            offsets.record(*time.offset());
            let time = time.with_timezone(&Utc);

            let mut values = HashMap::new();
            for (i, param_name) in parameter_names.iter().enumerate() {
                let val = record[param_start + i].parse::<f64>().ok();
                values.insert(param_name.clone(), val);
            }

            let idx = rows.len();
            location_index
                .entry(location.clone())
                .or_default()
                .push(idx);
            time_index
                .entry(location.clone())
                .or_default()
                .entry(time)
                .or_default()
                .push(idx);

            rows.push(CsvRow {
                location,
                latitude,
                longitude,
                time,
                values,
            });
        }

        if let Some(mix) = offsets.mixed() {
            tracing::warn!(
                "CSV {path}: `time` column mixes {} UTC offsets (rows per offset: {mix}); \
                 every row was converted to UTC",
                offsets.distinct()
            );
        }

        Ok(CsvDataStore {
            rows,
            location_index,
            time_index,
            parameter_names,
            parameter_units,
        })
    }
}

/// Parses the `time` column: RFC 3339 with a mandatory UTC offset (`Z`,
/// `UTC`, `+02:00`, `+0200`; a space may replace the `T`). A naive
/// timestamp is a load error — it is never assumed to be UTC.
fn parse_time(s: &str) -> Result<DateTime<FixedOffset>, DataServerError> {
    s.parse()
        .map_err(|e| DataServerError::Engine(format!("Invalid time: {e}")))
}

/// Rows per distinct UTC offset in one file's `time` column, keyed by
/// seconds east of UTC, so `Z`, `UTC` and `+00:00` are one offset. Every
/// offset converts to UTC correctly; a file mixing them is still worth a
/// WARN, since it usually means concatenated exports or local time across
/// a DST switch (#23).
#[derive(Debug, Default)]
struct OffsetTally(BTreeMap<i32, (FixedOffset, usize)>);

impl OffsetTally {
    fn record(&mut self, offset: FixedOffset) {
        self.0
            .entry(offset.local_minus_utc())
            .or_insert((offset, 0))
            .1 += 1;
    }

    fn distinct(&self) -> usize {
        self.0.len()
    }

    /// `None` for a file with one consistent offset; otherwise each offset
    /// with its row count, ascending: `+00:00=3, +02:00=1`.
    fn mixed(&self) -> Option<String> {
        (self.distinct() > 1).then(|| {
            self.0
                .values()
                .map(|(offset, rows)| format!("{offset}={rows}"))
                .collect::<Vec<_>>()
                .join(", ")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tally(times: &[&str]) -> OffsetTally {
        let mut offsets = OffsetTally::default();
        for t in times {
            offsets.record(*parse_time(t).unwrap().offset());
        }
        offsets
    }

    #[test]
    fn mixed_offsets_are_reported_with_row_counts() {
        let offsets = tally(&[
            "2026-06-01T00:00:00Z",
            "2026-06-01T03:00:00+03:00",
            "2026-06-01T01:00:00+00:00",
            "2026-06-01 02:00:00-05:30",
            "2026-06-01T02:00:00UTC",
        ]);
        assert_eq!(offsets.distinct(), 3);
        assert_eq!(
            offsets.mixed().as_deref(),
            Some("-05:30=1, +00:00=3, +03:00=1")
        );
    }

    #[test]
    fn one_consistent_offset_is_not_flagged() {
        assert_eq!(tally(&[]).mixed(), None);
        // `Z`, `UTC`, `+00:00` and `+0000` are all the same offset.
        let utc = tally(&[
            "2026-06-01T00:00:00Z",
            "2026-06-01T01:00:00UTC",
            "2026-06-01T02:00:00+00:00",
            "2026-06-01T03:00:00+0000",
        ]);
        assert_eq!(utc.distinct(), 1);
        assert_eq!(utc.mixed(), None);
        let local = tally(&["2026-06-01T03:00:00+03:00", "2026-06-01 04:00:00+0300"]);
        assert_eq!(local.mixed(), None);
    }

    /// Offsets only annotate the instant: conversion to UTC is unchanged.
    #[test]
    fn offsets_convert_to_the_same_utc_instant() {
        let utc = |s: &str| parse_time(s).unwrap().with_timezone(&Utc);
        assert_eq!(
            utc("2026-06-01T03:00:00+03:00"),
            utc("2026-06-01T00:00:00Z")
        );
        assert_eq!(
            utc("2026-06-01T03:00:00+03:00"),
            "2026-06-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    /// A naive timestamp cannot join the mix: it fails the load rather than
    /// being read as UTC. If this ever parses, `OffsetTally` must learn to
    /// report naive rows too.
    #[test]
    fn naive_timestamp_is_rejected() {
        assert!(parse_time("2026-06-01T00:00:00").is_err());
        assert!(parse_time("2026-06-01 00:00:00").is_err());
    }
}
