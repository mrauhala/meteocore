//! One time slice of the source station collection: every station's latest
//! value of a parameter not after the analysis time and at most `max_age`
//! old, read with one EDR area query over the grid's search box (the
//! set-based "every station at one time" read, #1062).

use std::hash::{Hash, Hasher};

use chrono::{DateTime, Duration, Utc};
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::model::{CoverageResponse, DomainDescription, QueryResult};

use crate::interpolate::Station;

/// The stations of one slice, in a canonical order, and a fingerprint of
/// what they hold.
pub struct Slice {
    /// Each station's position and observed value.
    pub stations: Vec<Station>,
    /// Changes whenever a station, its position or its value does: late
    /// and corrected observations move it (`content_version`).
    pub fingerprint: u64,
    /// Per station, what the lapse-rate correction takes off its value
    /// before interpolation, `Γ·z_station` (#1206); empty without one.
    pub reduction: Vec<f64>,
}

impl Slice {
    /// The stations as the grid interpolates them: each value less its
    /// reduction.
    pub fn interpolated(&self) -> std::borrow::Cow<'_, [Station]> {
        if self.reduction.is_empty() {
            return std::borrow::Cow::Borrowed(&self.stations);
        }
        std::borrow::Cow::Owned(
            self.stations
                .iter()
                .zip(&self.reduction)
                .map(|(s, r)| Station {
                    value: s.value - r,
                    ..*s
                })
                .collect(),
        )
    }
}

/// Read the slice of `parameter` (the source's own spelling) at `time` from
/// the stations inside `bbox` (`west > east` crosses the antimeridian).
///
/// Synchronous source I/O: call it on the background poll runtime or the
/// grid pool, never on a request worker.
pub fn fetch(
    source: &dyn EdrEngine,
    parameter: &str,
    bbox: [f64; 4],
    time: DateTime<Utc>,
    max_age: Duration,
) -> Result<Slice, DataServerError> {
    let [west, south, east, north] = bbox;
    let coords = format!("{west},{south},{east},{north}");
    let window = (time - max_age, time);
    let names = [parameter.to_string()];
    let response = match source.query_area(&coords, Some(window), Some(&names), None, None) {
        Ok(response) => response,
        // No station in the box, or none with data in the window: an empty
        // slice, which grids to nodata.
        Err(DataServerError::LocationNotFound(_)) => CoverageResponse::Collection(Vec::new()),
        Err(e) => return Err(e),
    };
    let results = match response {
        CoverageResponse::Single(result) => vec![result],
        CoverageResponse::Collection(results) => results,
    };
    let mut stations: Vec<Station> = results
        .iter()
        .filter_map(|result| latest(result, parameter, window))
        .collect();
    stations.sort_by(|a, b| {
        a.lon
            .total_cmp(&b.lon)
            .then(a.lat.total_cmp(&b.lat))
            .then(a.value.total_cmp(&b.value))
    });
    Ok(Slice {
        fingerprint: fingerprint(&stations),
        stations,
        reduction: Vec::new(),
    })
}

/// A station coverage's latest finite value inside `window`, both ends
/// included.
fn latest(
    result: &QueryResult,
    parameter: &str,
    (start, end): (DateTime<Utc>, DateTime<Utc>),
) -> Option<Station> {
    let values = &result.ranges.get(parameter)?.values;
    let in_window = |t: &DateTime<Utc>| start <= *t && *t <= end;
    let (lon, lat, value) = match &result.domain {
        DomainDescription::PointSeries { x, y, t, .. } => {
            let value = t
                .iter()
                .zip(values)
                .filter(|(t, v)| in_window(t) && v.is_some_and(f64::is_finite))
                .max_by_key(|(t, _)| **t)
                .and_then(|(_, v)| *v)?;
            (*x, *y, value)
        }
        DomainDescription::Point { x, y, t, .. } => {
            let value = values.first().copied().flatten()?;
            if !(value.is_finite() && t.as_ref().is_none_or(in_window)) {
                return None;
            }
            (*x, *y, value)
        }
        _ => return None,
    };
    (lon.is_finite() && lat.is_finite() && (-90.0..=90.0).contains(&lat)).then_some(Station {
        lon,
        lat,
        value,
    })
}

fn fingerprint(stations: &[Station]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    stations.len().hash(&mut hasher);
    for s in stations {
        s.lon.to_bits().hash(&mut hasher);
        s.lat.to_bits().hash(&mut hasher);
        s.value.to_bits().hash(&mut hasher);
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use ds_core::model::NdArray;
    use std::collections::HashMap;

    fn series(x: f64, y: f64, t: Vec<DateTime<Utc>>, values: Vec<Option<f64>>) -> QueryResult {
        QueryResult {
            domain: DomainDescription::PointSeries { x, y, t, z: None },
            parameters: HashMap::new(),
            ranges: HashMap::from([(
                "t2m".to_string(),
                NdArray {
                    shape: vec![values.len()],
                    axis_names: vec!["t".into()],
                    values,
                },
            )]),
        }
    }

    fn hour(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, h, m, 0).unwrap()
    }

    /// The latest finite value inside the window wins; later, older, missing
    /// and non-finite values do not.
    #[test]
    fn latest_value_in_the_window() {
        let window = (hour(11, 0), hour(12, 0));
        let s = series(
            24.0,
            60.0,
            vec![
                hour(10, 0),
                hour(11, 30),
                hour(11, 50),
                hour(12, 0),
                hour(12, 10),
            ],
            vec![Some(1.0), Some(2.0), Some(3.0), Some(f64::NAN), Some(5.0)],
        );
        assert_eq!(latest(&s, "t2m", window).unwrap().value, 3.0);
        let stale = series(24.0, 60.0, vec![hour(10, 0)], vec![Some(1.0)]);
        assert!(latest(&stale, "t2m", window).is_none());
        let boundary = series(24.0, 60.0, vec![hour(11, 0)], vec![Some(4.0)]);
        assert_eq!(latest(&boundary, "t2m", window).unwrap().value, 4.0);
        assert!(latest(&s, "other", window).is_none());
    }

    #[test]
    fn fingerprint_moves_with_any_value() {
        let a = [Station {
            lon: 1.0,
            lat: 2.0,
            value: 3.0,
        }];
        let mut b = a;
        b[0].value = 3.5;
        assert_ne!(fingerprint(&a), fingerprint(&b));
        assert_eq!(fingerprint(&a), fingerprint(&a.clone()));
        assert_ne!(fingerprint(&a), fingerprint(&[]));
    }
}
