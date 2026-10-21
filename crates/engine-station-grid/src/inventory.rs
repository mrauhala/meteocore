//! What the source's station inventory says about each station (#1206):
//! its stated height and whether `exclude_where` excludes it. Read through
//! the source's existing `FeatureEngine` (BUFR stations carry `elevation`,
//! a PostGIS registry whatever its `property_cols` name, CSV only `name`)
//! by the poll loop, never on a request, and matched to a slice's stations
//! by position: the EDR area answer carries no station id.

use std::collections::HashMap;

use ds_core::config::StationExclusions;
use ds_core::error::DataServerError;
use ds_core::feature::{matches_property_values, FeatureQuery, Geometry, PropertyValue};
use ds_core::feature_engine::FeatureEngine;

use crate::geometry::wrap_lon;

/// The station property holding a station's height above sea level in
/// metres: BUFR's (WMO Table B 007030/007001), and the name a PostGIS
/// registry column or view gives it.
pub const HEIGHT_PROPERTY: &str = "elevation";

/// Features asked for per page.
const PAGE: usize = 10_000;

/// A position key: longitude (wrapped) and latitude to 1e-6°, about 0.1 m.
pub(crate) type Key = (i64, i64);

pub(crate) fn key(lon: f64, lat: f64) -> Key {
    (
        (wrap_lon(lon) * 1e6).round() as i64,
        (lat * 1e6).round() as i64,
    )
}

/// One station of the inventory.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Known {
    /// Metres above sea level, as the source states it.
    pub stated_height: Option<f64>,
    pub excluded: bool,
}

/// The inventory as of one source data version.
#[derive(Debug, Default)]
pub(crate) struct Inventory {
    /// The source's `FeatureEngine::data_version` it was read at; `None`
    /// before the first read.
    pub version: Option<u64>,
    pub stations: HashMap<Key, Known>,
}

impl Inventory {
    pub fn get(&self, lon: f64, lat: f64) -> Option<Known> {
        self.stations.get(&key(lon, lat)).copied()
    }
}

/// `exclude_where` as filters for the shared Features property matcher:
/// rules → properties → alternatives.
#[derive(Debug, Default)]
pub(crate) struct Exclusions(Vec<Vec<Vec<(String, String)>>>);

impl Exclusions {
    pub fn new(config: &StationExclusions) -> Result<Self, String> {
        let mut rules = Vec::new();
        for rule in &config.0 {
            if rule.is_empty() {
                return Err("exclude_where has a rule naming no property".into());
            }
            let mut properties = Vec::new();
            for (property, values) in rule {
                if property.trim().is_empty() {
                    return Err("exclude_where names an empty property".into());
                }
                if values.is_empty() {
                    return Err(format!(
                        "exclude_where property '{property}' lists no values"
                    ));
                }
                properties.push(
                    values
                        .iter()
                        .map(|v| (property.clone(), v.filter_text()))
                        .collect(),
                );
            }
            rules.push(properties);
        }
        Ok(Self(rules))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether a station with `properties` matches any rule: each property
    /// a rule names equals one of its values.
    pub fn excludes(&self, properties: &HashMap<String, PropertyValue>) -> bool {
        self.0.iter().any(|rule| {
            rule.iter().all(|alternatives| {
                alternatives
                    .iter()
                    .any(|filter| matches_property_values(properties, std::slice::from_ref(filter)))
            })
        })
    }
}

/// Every station of `engine`'s inventory by position. Where several share a
/// position, the first stated height is kept and any exclusion excludes.
pub(crate) fn read(
    engine: &dyn FeatureEngine,
    exclusions: &Exclusions,
) -> Result<HashMap<Key, Known>, DataServerError> {
    let mut stations: HashMap<Key, Known> = HashMap::new();
    let mut offset = 0;
    loop {
        let page = engine.get_features(&FeatureQuery {
            limit: PAGE,
            offset,
            ..FeatureQuery::default()
        })?;
        for feature in &page.features {
            let Geometry::Point { x, y } = *feature.geometry else {
                continue;
            };
            if !(x.is_finite() && y.is_finite()) {
                continue;
            }
            let stated_height = match feature.properties.get(HEIGHT_PROPERTY) {
                Some(PropertyValue::Float(v)) if v.is_finite() => Some(*v),
                Some(PropertyValue::Integer(v)) => Some(*v as f64),
                _ => None,
            };
            let excluded = exclusions.excludes(&feature.properties);
            let known = stations.entry(key(x, y)).or_default();
            known.stated_height = known.stated_height.or(stated_height);
            known.excluded |= excluded;
        }
        match page.next_offset {
            Some(next) if next > offset && !page.features.is_empty() => offset = next,
            _ => break,
        }
    }
    Ok(stations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::config::{StationExclusionRule, StationPropertyValue};

    fn props(pairs: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn exclusion_rules_match_like_features_filters() {
        let config = StationExclusions(vec![
            StationExclusionRule::from([(
                "exclude_from_gridding".to_string(),
                vec![StationPropertyValue::Bool(true)],
            )]),
            StationExclusionRule::from([
                (
                    "network".to_string(),
                    vec![
                        StationPropertyValue::Text("SHIP".into()),
                        StationPropertyValue::Text("BUOY".into()),
                    ],
                ),
                ("height".to_string(), vec![StationPropertyValue::Integer(2)]),
            ]),
        ]);
        let e = Exclusions::new(&config).unwrap();
        assert!(e.excludes(&props(&[(
            "exclude_from_gridding",
            PropertyValue::Bool(true)
        )])));
        assert!(!e.excludes(&props(&[(
            "exclude_from_gridding",
            PropertyValue::Bool(false)
        )])));
        // Both properties of the second rule must match.
        assert!(e.excludes(&props(&[
            ("network", PropertyValue::String("BUOY".into())),
            ("height", PropertyValue::Float(2.0)),
        ])));
        assert!(!e.excludes(&props(&[("network", PropertyValue::String("BUOY".into()))])));
        assert!(!e.excludes(&props(&[])));

        let empty_rule = StationExclusions(vec![StationExclusionRule::new()]);
        assert!(Exclusions::new(&empty_rule)
            .unwrap_err()
            .contains("no property"));
        let no_values = StationExclusions(vec![StationExclusionRule::from([(
            "a".to_string(),
            vec![],
        )])]);
        assert!(Exclusions::new(&no_values)
            .unwrap_err()
            .contains("'a' lists no values"));
    }

    #[test]
    fn keys_wrap_the_antimeridian() {
        assert_eq!(key(180.0, 10.0), key(-180.0, 10.0));
        assert_eq!(key(24.0000001, 60.0), key(24.0, 60.0));
        assert_ne!(key(24.00001, 60.0), key(24.0, 60.0));
    }
}
