//! The collection's `FeatureEngine` (#1206): the leave-one-out residuals of
//! its analyses, one Point feature per (station, parameter, analysis time),
//! with the properties `parameter`, `time`, `value`, `estimate`, `residual`
//! and `neighbours` (see [`crate::residuals`]).
//!
//! - **Time.** A feature's time is its analysis time, an instant, so
//!   `datetime` (OGC: the instants inside the interval, ends included)
//!   selects analysis times. An instant that is not one selects none.
//!   Without `datetime`, the latest analysis time.
//! - **Cap.** One request reads at most [`MAX_QUERY_GRIDS`] analyses
//!   (times × parameters), the EDR cap; more is `QueryTooLarge` (400). A
//!   `parameter` or `time` property filter narrows what is read.
//! - **Reads.** The residuals are cached with each grid, computed with it on
//!   the station-grid pool. A request reads them through the start/finish
//!   path a map uses: hits are served, misses are queued (single-flight,
//!   bytes reserved) and waited for until the request deadline. A Features
//!   request runs on a request worker without a deadline, which must not
//!   block, so it does not wait at all: a miss is `ResourceExhausted`
//!   (503, `Retry-After`) and its computation goes on to serve the retry.
//!   Never a partial page. The poll keeps the latest `precompute` times
//!   cached, so the default request is a hit.
//! - **Paging.** Filters (`bbox`, then the property filters) apply before
//!   counting; `sortby` sorts every match before paging. Without `sortby`
//!   only the page's features are built.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use chrono::{DateTime, SecondsFormat, Utc};
use ds_core::error::DataServerError;
use ds_core::feature::{
    matches_property_filters, sort_features, DatetimeInterval, Feature, FeaturePage, FeatureQuery,
    FilterableProperties, Geometry, PropertyValue,
};
use ds_core::feature_engine::FeatureEngine;
use ds_core::map_engine::MapEngine;

use crate::compute::AnalysisGrid;
use crate::engine::{StationGridEngine, MAX_QUERY_GRIDS};
use crate::residuals::Residual;

/// Every property a residual feature carries; each can be a filter.
const PROPERTIES: [&str; 6] = [
    "parameter",
    "time",
    "value",
    "estimate",
    "residual",
    "neighbours",
];

const SORTABLES: &[&str] = &[
    "residual",
    "estimate",
    "value",
    "neighbours",
    "time",
    "parameter",
];

/// The analysis times `datetime` selects: the latest without one, else
/// every time inside the interval (ends included, open ends unbounded).
fn select_times(
    times: &[DateTime<Utc>],
    datetime: Option<&DatetimeInterval>,
) -> Vec<DateTime<Utc>> {
    match datetime {
        None => times.last().copied().into_iter().collect(),
        Some(interval) => {
            let lo = interval
                .start
                .map_or(0, |s| times.partition_point(|t| *t < s));
            let hi = interval
                .end
                .map_or(times.len(), |e| times.partition_point(|t| *t <= e));
            times[lo..hi.max(lo)].to_vec()
        }
    }
}

/// An analysis time as the `time` property and feature ids spell it.
fn time_text(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Whether `text` passes every `name` property filter: an exact string,
/// as [`matches_property_filters`] compares a string property.
fn passes(filters: &[(String, String)], name: &str, text: &str) -> bool {
    filters
        .iter()
        .filter(|(n, _)| n == name)
        .all(|(_, expected)| expected == text)
}

/// A feature id: `<parameter>@<time>@<lon>,<lat>`, with `~<n>` after the
/// `n`-th further station at the same position in the analysis. Positions
/// are the shortest decimals that read back as the same `f64`.
fn feature_id(parameter: &str, time: &str, r: &Residual, twin: usize) -> String {
    let (lon, lat) = (r.lon, r.lat);
    match twin {
        0 => format!("{parameter}@{time}@{lon},{lat}"),
        n => format!("{parameter}@{time}@{lon},{lat}~{n}"),
    }
}

/// `(parameter, time, lon, lat, twin)` of an id, `None` when it is not one.
fn parse_id(id: &str) -> Option<(&str, DateTime<Utc>, f64, f64, usize)> {
    let mut parts = id.rsplitn(3, '@');
    let position = parts.next()?;
    let time = parts.next()?;
    let parameter = parts.next()?;
    let (position, twin) = match position.split_once('~') {
        Some((position, n)) => (position, n.parse::<usize>().ok().filter(|&n| n > 0)?),
        None => (position, 0),
    };
    let (lon, lat) = position.split_once(',')?;
    let time = DateTime::parse_from_rfc3339(time).ok()?.with_timezone(&Utc);
    Some((parameter, time, lon.parse().ok()?, lat.parse().ok()?, twin))
}

/// The residual features of one analysis, in slice order, with their
/// twin numbers.
fn rows<'a>(grid: &'a AnalysisGrid) -> impl Iterator<Item = (&'a Residual, usize)> + 'a {
    // The slice is sorted by position, so stations sharing one are adjacent.
    let mut previous: Option<(f64, f64)> = None;
    let mut twin = 0usize;
    grid.residuals.iter().map(move |r| {
        twin = match previous {
            Some(p) if p == (r.lon, r.lat) => twin + 1,
            _ => 0,
        };
        previous = Some((r.lon, r.lat));
        (r, twin)
    })
}

fn build(parameter: &str, time: &str, r: &Residual, twin: usize) -> Feature {
    let float = |v: Option<f64>| v.map_or(PropertyValue::Null, PropertyValue::Float);
    let properties = HashMap::from([
        (
            "parameter".to_string(),
            PropertyValue::String(parameter.to_string()),
        ),
        ("time".to_string(), PropertyValue::String(time.to_string())),
        ("value".to_string(), PropertyValue::Float(r.value)),
        ("estimate".to_string(), float(r.estimate)),
        ("residual".to_string(), float(r.residual())),
        (
            "neighbours".to_string(),
            PropertyValue::Integer(i64::from(r.neighbours)),
        ),
    ]);
    Feature {
        id: feature_id(parameter, time, r, twin),
        geometry: Arc::new(Geometry::Point { x: r.lon, y: r.lat }),
        properties: Arc::new(properties),
        related: Vec::new(),
    }
}

impl StationGridEngine {
    /// The analyses of `params × times`, read without blocking a request
    /// worker: waiting until the caller's deadline, or not at all without
    /// one (module docs).
    fn residual_grids(
        &self,
        params: &[usize],
        times: &[DateTime<Utc>],
    ) -> Result<Vec<Vec<Arc<AnalysisGrid>>>, DataServerError> {
        let _no_wait = ds_core::deadline::current()
            .is_none()
            .then(|| ds_core::deadline::enter(Some(Instant::now())));
        self.grids_for(params, times)
    }
}

/// Leave-one-out residuals as features; see the module docs.
impl FeatureEngine for StationGridEngine {
    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        let filters = &query.property_filters;
        let names = self.parameter_names();
        // A `parameter` or `time` filter only narrows what is read: every
        // feature is still matched against all the filters below.
        let params: Vec<usize> = (0..names.len())
            .filter(|&p| passes(filters, "parameter", &names[p]))
            .collect();
        let times: Vec<DateTime<Utc>> =
            select_times(&self.analysis_times(), query.datetime.as_ref())
                .into_iter()
                .filter(|&t| passes(filters, "time", &time_text(t)))
                .collect();
        let analyses = params.len() * times.len();
        if analyses > MAX_QUERY_GRIDS {
            return Err(DataServerError::QueryTooLarge(format!(
                "the request reads {analyses} analyses ({} analysis times × {} parameters); \
                 the limit is {MAX_QUERY_GRIDS}: narrow `datetime`, or filter by `parameter`",
                times.len(),
                params.len()
            )));
        }
        let grids = if analyses == 0 {
            Vec::new()
        } else {
            self.residual_grids(&params, &times)?
        };

        let sorting = !query.sortby.is_empty();
        let checking = sorting || !filters.is_empty();
        // Sorting: every match. Otherwise: the page alone.
        let mut kept: Vec<Feature> = Vec::new();
        let mut matched = 0usize;
        for (&p, grids) in params.iter().zip(&grids) {
            let parameter = &names[p];
            for (&time, grid) in times.iter().zip(grids) {
                let time = time_text(time);
                for (r, twin) in rows(grid) {
                    if query
                        .bbox
                        .as_ref()
                        .is_some_and(|b| !b.contains(r.lon, r.lat))
                    {
                        continue;
                    }
                    let feature = if checking {
                        let feature = build(parameter, &time, r, twin);
                        if !matches_property_filters(&feature, filters) {
                            continue;
                        }
                        Some(feature)
                    } else {
                        None
                    };
                    if sorting {
                        kept.extend(feature);
                    } else if matched >= query.offset && kept.len() < query.limit {
                        kept.push(feature.unwrap_or_else(|| build(parameter, &time, r, twin)));
                    }
                    matched += 1;
                }
            }
        }
        let features: Vec<Feature> = if sorting {
            sort_features(&mut kept, &query.sortby);
            kept.into_iter()
                .skip(query.offset)
                .take(query.limit)
                .collect()
        } else {
            kept
        };
        let number_returned = features.len();
        let end = query.offset.saturating_add(number_returned);
        Ok(FeaturePage {
            features,
            number_matched: matched,
            number_returned,
            next_offset: (end < matched).then_some(end),
        })
    }

    /// The feature of an id `get_features` gave, read like a page. An id of
    /// no station in that analysis is `FeatureNotFound`.
    fn get_feature(&self, feature_id: &str) -> Result<Feature, DataServerError> {
        let not_found = || DataServerError::FeatureNotFound(feature_id.to_string());
        let (parameter, time, lon, lat, twin) = parse_id(feature_id).ok_or_else(not_found)?;
        let p = self
            .parameter_names()
            .iter()
            .position(|name| name == parameter)
            .ok_or_else(not_found)?;
        if self.analysis_times().binary_search(&time).is_err() {
            return Err(not_found());
        }
        let grid = self
            .residual_grids(&[p], &[time])?
            .into_iter()
            .flatten()
            .next()
            .ok_or_else(not_found)?;
        let text = time_text(time);
        let found = rows(&grid)
            .find(|(r, n)| r.lon == lon && r.lat == lat && *n == twin)
            .map(|(r, n)| build(parameter, &text, r, n));
        found.ok_or_else(not_found)
    }

    /// The stations of each parameter's latest analysis computed, summed:
    /// O(1), never a computation for collection metadata.
    fn feature_count(&self) -> usize {
        self.station_counts()
            .into_iter()
            .filter_map(|(_, stations)| stations)
            .sum()
    }

    fn sortables(&self) -> &[&'static str] {
        SORTABLES
    }

    fn filterables(&self) -> FilterableProperties {
        static NAMES: LazyLock<FilterableProperties> =
            LazyLock::new(|| Arc::new(PROPERTIES.iter().map(|n| n.to_string()).collect()));
        NAMES.clone()
    }

    /// `temperature at 60.25, 24.95`: the parameter and the station's
    /// position, both values the feature carries.
    fn label(&self, feature: &Feature) -> Option<String> {
        let parameter = feature.properties.get("parameter")?.as_str()?;
        match feature.geometry.as_ref() {
            Geometry::Point { x, y } => Some(format!("{parameter} at {y}, {x}")),
            _ => None,
        }
    }

    fn summary_properties(&self) -> &[&'static str] {
        &["residual", "value", "estimate", "neighbours"]
    }

    fn items_description(&self) -> Option<&'static str> {
        Some(
            "Leave-one-out residuals: one point per station, parameter and analysis time. \
             `estimate` is the analysis at the station from the other stations, `residual` \
             that estimate minus the station's `value`.",
        )
    }

    /// The box the analyses read their stations from.
    fn spatial_extent(&self) -> Option<[f64; 4]> {
        Some(self.search_bbox())
    }

    fn temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let times = self.analysis_times();
        Some((*times.first()?, *times.last()?))
    }

    fn available_times(&self) -> Vec<DateTime<Utc>> {
        self.analysis_times().to_vec()
    }

    /// Moves with every revision of the observations, as the maps' content
    /// version does.
    fn data_version(&self) -> u64 {
        self.content_version()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, h, 0, 0).unwrap()
    }

    #[test]
    fn datetime_selects_analysis_times() {
        let times = [at(0), at(1), at(2), at(3)];
        let interval = |start, end| DatetimeInterval { start, end };
        assert_eq!(select_times(&times, None), vec![at(3)]);
        assert_eq!(
            select_times(&times, Some(&interval(Some(at(1)), Some(at(1))))),
            vec![at(1)]
        );
        let between = at(1) + chrono::Duration::minutes(30);
        assert!(select_times(&times, Some(&interval(Some(between), Some(between)))).is_empty());
        assert_eq!(
            select_times(&times, Some(&interval(Some(at(1)), Some(at(2))))),
            vec![at(1), at(2)]
        );
        assert_eq!(
            select_times(&times, Some(&interval(None, Some(at(1))))),
            vec![at(0), at(1)]
        );
        assert_eq!(
            select_times(&times, Some(&interval(Some(at(2)), None))),
            vec![at(2), at(3)]
        );
        assert!(select_times(&[], None).is_empty());
        assert!(select_times(&times, Some(&interval(Some(at(3)), Some(at(1))))).is_empty());
    }

    #[test]
    fn ids_read_back() {
        let r = Residual {
            lon: -179.25,
            lat: 63.089,
            value: 1.0,
            estimate: None,
            neighbours: 0,
        };
        let time = time_text(at(11));
        for twin in [0, 2] {
            let id = feature_id("t@2m", &time, &r, twin);
            let (parameter, t, lon, lat, n) = parse_id(&id).unwrap();
            assert_eq!(
                (parameter, t, lon, lat, n),
                ("t@2m", at(11), -179.25, 63.089, twin)
            );
        }
        assert_eq!(
            feature_id("t2m", &time, &r, 0),
            "t2m@2026-01-01T11:00:00Z@-179.25,63.089"
        );
        for bad in [
            "",
            "t2m",
            "t2m@2026-01-01T11:00:00Z",
            "t2m@yesterday@1,2",
            "t2m@2026-01-01T11:00:00Z@1",
            "t2m@2026-01-01T11:00:00Z@1,2~0",
            "t2m@2026-01-01T11:00:00Z@1,2~x",
        ] {
            assert!(parse_id(bad).is_none(), "{bad}");
        }
    }
}
