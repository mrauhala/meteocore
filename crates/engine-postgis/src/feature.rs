//! `FeatureEngine` impl for `PostgisEngine`: feature = station on the
//! station shapes, feature = event on the `events` shape (#503).
//!
//! Stations: feature metadata (id, geometry, properties) lives in the
//! [`MetadataCache`]; feature lookups therefore never touch the pool.
//! Properties are already coerced from `pg_type` at refresh time —
//! arrays, `json`/`jsonb`, enums, timestamps, and any other types that
//! don't map cleanly to [`PropertyValue`] are rejected at startup so
//! operators see the mismatch early.
//!
//! Station paging semantics mirror `CsvEngine::get_features`: `bbox` +
//! `limit` + `offset`; `number_matched` counts the full filtered set
//! (pre-paging) so consumers can compute total pages.
//!
//! `feature = station` is inherently time-agnostic in v1 (temporal
//! features are an explicit non-goal per the plan doc), so
//! `has_time_dimension` is false and the API layer answers a `datetime`
//! filter with 400 rather than ignoring it.
//!
//! Events are read from the table per request
//! ([`PostgisEngine::event_features`]), so the engine says so
//! (`reads_on_request`: the API runs it on the query runtime) and pages by
//! a keyset cursor on the table's `(time, id)` key ([`EventKey`]).
//!
//! [`PropertyValue`]: ds_core::feature::PropertyValue

use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_core::feature::{Bbox, Feature, FeaturePage, FeatureQuery, Geometry};
use ds_core::feature_engine::FeatureEngine;

use crate::engine::PostgisEngine;
use crate::metadata::FeatureStation;

/// An event's key (#503): its time and its `id_col` value as text, the
/// table's `(time, id)` key. It is the event's feature id and the cursor of
/// the page after it, written `<id>@<time>`, the time RFC 3339 UTC with as
/// many fraction digits as it has (the `<parameter>@<time>@…` form of the
/// station grid's ids). The id may hold `@` itself: the time follows the
/// last one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventKey {
    pub time: DateTime<Utc>,
    pub id: String,
}

impl EventKey {
    /// The key a feature id or cursor writes, exactly as it writes it; `None`
    /// for any other string, before any query. An id is never a NUL (no
    /// PostgreSQL text holds one, so binding it would be a database error),
    /// and the time is the canonical form: `9@2026-01-01T07:30:00+02:00`
    /// names the same instant as a key does but is not a key, so one event
    /// has one id. The id part's own canonical form is its column's
    /// ([`crate::engine`]'s by-key lookup).
    pub fn parse(s: &str) -> Option<Self> {
        let (id, time) = s.rsplit_once('@')?;
        if id.contains('\0') {
            return None;
        }
        let time = DateTime::parse_from_rfc3339(time).ok()?.with_timezone(&Utc);
        let key = Self {
            time,
            id: id.to_string(),
        };
        (key.to_string() == s).then_some(key)
    }
}

impl std::fmt::Display for EventKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.id, crate::engine::rfc3339(self.time))
    }
}

impl FeatureEngine for PostgisEngine {
    fn filterables(&self) -> ds_core::feature::FilterableProperties {
        self.cache().load().filterables.clone()
    }

    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        if let Some(shape) = self.config().events() {
            return self.event_features(shape, query);
        }
        self.reject_invalid_schema()?;
        let meta = self.cache().load();
        let filtered: Vec<&FeatureStation> = meta
            .feature_stations
            .iter()
            .filter(|s| bbox_contains(query.bbox.as_ref(), s.lon, s.lat))
            .filter(|s| {
                ds_core::feature::matches_property_values(&s.properties, &query.property_filters)
            })
            .collect();

        let number_matched = filtered.len();
        let offset = query.offset.min(number_matched);
        let limit = if query.limit == 0 {
            number_matched.saturating_sub(offset)
        } else {
            query.limit
        };
        let slice = &filtered[offset..(offset + limit).min(number_matched)];

        let features: Vec<Feature> = slice.iter().map(|s| station_to_feature(s)).collect();

        let number_returned = features.len();
        let next_offset = if offset + number_returned < number_matched {
            Some(offset + number_returned)
        } else {
            None
        };

        Ok(FeaturePage {
            features,
            number_matched: Some(number_matched),
            number_returned,
            next_offset,
            next_cursor: None,
        })
    }

    fn get_feature(&self, feature_id: &str) -> Result<Feature, DataServerError> {
        if let Some(shape) = self.config().events() {
            return self.event_feature(shape, feature_id);
        }
        self.reject_invalid_schema()?;
        let meta = self.cache().load();
        let i = *meta
            .station_idx
            .get(feature_id)
            .ok_or_else(|| DataServerError::FeatureNotFound(feature_id.to_string()))?;
        Ok(station_to_feature(&meta.feature_stations[i]))
    }

    /// The cached stations; never a database count, so an event table,
    /// which the snapshot does not count, is `0` and leaves `numberItems`
    /// out ([`Self::number_items`]).
    fn feature_count(&self) -> usize {
        self.cache().load().feature_stations.len()
    }

    fn number_items(&self) -> Option<usize> {
        self.config()
            .events()
            .is_none()
            .then(|| self.feature_count())
    }

    fn pages_by_cursor(&self) -> bool {
        self.config().events().is_some()
    }

    fn reads_on_request(&self) -> bool {
        self.config().events().is_some()
    }

    fn summary_properties(&self) -> &[&'static str] {
        if self.config().events().is_some() {
            &["time"]
        } else {
            &[]
        }
    }

    fn items_description(&self) -> Option<&'static str> {
        self.config()
            .events()
            .map(|_| "One point per event, newest first. Without datetime, the events of the collection's default window ending now.")
    }

    fn spatial_extent(&self) -> Option<[f64; 4]> {
        self.cache().load().spatial_extent
    }

    /// Events: the first event to the latest one not after the last
    /// refresh. The station inventory has none.
    fn temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.config()
            .events()
            .and_then(|_| self.cache().load().temporal_extent)
    }

    /// Events carry their time and `datetime` selects them (#503). The
    /// station inventory has no time dimension: `datetime` is a 400 (#682).
    /// Observations are EDR's.
    fn has_time_dimension(&self) -> bool {
        self.config().events().is_some()
    }

    /// Events: the `default_datetime` window ending `now`, the one EDR's
    /// area query reads without `datetime`, named so the Features API fixes
    /// it for a whole `next` chain (#503).
    fn default_datetime(&self, now: DateTime<Utc>) -> Option<ds_core::feature::DatetimeInterval> {
        self.config().events()?;
        let (start, end) = self.events_default_range_at(now);
        Some(ds_core::feature::DatetimeInterval {
            start: Some(start),
            end: Some(end),
        })
    }

    fn data_version(&self) -> u64 {
        // The metadata cache bumps `version` on every successful refresh, so
        // station-set changes (and PR #110's planned background refresh) will
        // automatically invalidate vector-tile ETags.
        self.cache().load().version
    }
}

fn station_to_feature(s: &FeatureStation) -> Feature {
    Feature {
        id: s.id.clone(),
        geometry: Arc::new(Geometry::Point { x: s.lon, y: s.lat }),
        properties: s.properties.clone(),
        // The station is the EDR location of its id (#1143).
        related: vec![ds_core::feature::Related::edr_location(&s.id)],
    }
}

fn bbox_contains(bbox: Option<&Bbox>, lon: f64, lat: f64) -> bool {
    match bbox {
        None => true,
        Some(b) => b.contains(lon, lat),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::feature::PropertyValue;
    use std::collections::HashMap;

    fn mk_station(id: &str, lon: f64, lat: f64, territory: &str) -> FeatureStation {
        let mut props = HashMap::new();
        props.insert(
            "territory".to_string(),
            PropertyValue::String(territory.into()),
        );
        FeatureStation {
            id: id.into(),
            label: format!("Station {id}"),
            lat,
            lon,
            properties: Arc::new(props),
        }
    }

    #[test]
    fn station_to_feature_wraps_geometry() {
        let s = mk_station("s1", 24.9, 60.2, "Finland");
        let f = station_to_feature(&s);
        assert_eq!(f.id, "s1");
        match f.geometry.as_ref() {
            Geometry::Point { x, y } => {
                assert_eq!(*x, 24.9);
                assert_eq!(*y, 60.2);
            }
            _ => panic!("expected Point"),
        }
        assert_eq!(
            f.properties.get("territory"),
            Some(&PropertyValue::String("Finland".into()))
        );
        // The station is the EDR location of its id (#1143).
        assert_eq!(f.related, [ds_core::feature::Related::edr_location("s1")]);
    }

    #[test]
    fn orphan_station_maps_to_feature_with_empty_properties() {
        // Mode A/B orphans carry id-as-label and no properties; they must still
        // produce a valid Point Feature.
        let orphan = FeatureStation {
            id: "0-148-0-CHD64901}".into(),
            label: "0-148-0-CHD64901}".into(),
            lat: 13.84,
            lon: 20.85,
            properties: Arc::new(HashMap::new()),
        };
        let f = station_to_feature(&orphan);
        assert_eq!(f.id, "0-148-0-CHD64901}");
        assert!(f.properties.is_empty());
        match f.geometry.as_ref() {
            Geometry::Point { x, y } => {
                assert_eq!(*x, 20.85);
                assert_eq!(*y, 13.84);
            }
            _ => panic!("expected Point"),
        }
    }

    #[test]
    fn bbox_contains_filters_correctly() {
        let bbox = Bbox::new(0.0, 0.0, 10.0, 10.0).unwrap();
        assert!(bbox_contains(Some(&bbox), 5.0, 5.0));
        assert!(!bbox_contains(Some(&bbox), 15.0, 5.0));
        // None bbox accepts everything.
        assert!(bbox_contains(None, -90.0, 90.0));
    }

    /// #503: an event key writes `<id>@<time>` and reads back the same key,
    /// a microsecond time exactly, an id holding `@` whole.
    #[test]
    fn event_key_round_trips_through_its_id() {
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        for (key, written) in [
            (
                EventKey {
                    time: at("2026-01-01T05:30:00Z"),
                    id: "17".into(),
                },
                "17@2026-01-01T05:30:00Z",
            ),
            (
                EventKey {
                    time: at("2026-01-01T05:30:00.123456Z"),
                    id: "a@b".into(),
                },
                "a@b@2026-01-01T05:30:00.123456Z",
            ),
            (
                EventKey {
                    time: at("2026-01-01T05:30:00.120Z"),
                    id: String::new(),
                },
                "@2026-01-01T05:30:00.120Z",
            ),
        ] {
            assert_eq!(key.to_string(), written);
            assert_eq!(EventKey::parse(written), Some(key));
        }
    }

    #[test]
    fn event_key_rejects_what_no_event_key_writes() {
        for s in [
            "",
            "17",
            "17@",
            "17@yesterday",
            "17@2026-01-01",
            "17@2026-01-01T05:30:00",
            "2026-01-01T05:30:00Z",
            "17@2026-01-01T05:30:00Z@x",
            // The same instant written another way is no key (#503 review):
            // one event, one id.
            "9@2026-01-01T07:30:00+02:00",
            "9@2026-01-01T05:30:00.000Z",
            "9@2026-01-01T05:30:00.123000Z",
            "9@2026-01-01t05:30:00z",
            "9@2026-01-01T05:30:00+00:00",
            // No PostgreSQL text holds a NUL: binding one is a database
            // error, so it is no key, before any query.
            "\0@2026-01-01T05:30:00Z",
            "1\u{0}2@2026-01-01T05:30:00Z",
        ] {
            assert_eq!(EventKey::parse(s), None, "{s:?}");
        }
    }
}
