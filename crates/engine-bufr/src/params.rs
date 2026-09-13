//! Parameter table: which Table B element (+ period) feeds each EDR
//! parameter. Built-ins cover the SYNOP essentials; `[[bufr.parameters]]`
//! entries override by name or add.
//!
//! Units are the BUFR units, unconverted (K, Pa, m s-1, kg m-2, …): the
//! source metadata is authoritative and clients convert.

use std::collections::HashMap;

use ds_core::config::BufrParameterConfig;
use ds_core::model::ParameterDescription;
use tinybufr::XY;

use crate::decode::{xy_from_code, ObsReport};

/// One column of the observation store.
#[derive(Debug, Clone, PartialEq)]
pub struct ParamDef {
    pub name: String,
    pub descriptors: Vec<XY>,
    pub unit: String,
    pub label: String,
    pub observed_property: String,
    pub period_hours: Option<f64>,
}

struct Builtin {
    name: &'static str,
    descriptors: &'static [&'static str],
    unit: &'static str,
    label: &'static str,
    observed_property: &'static str,
    period_hours: Option<f64>,
}

/// The SYNOP essentials. Order = column order in the store and the order
/// `get_parameters` advertises.
const BUILTIN: &[Builtin] = &[
    Builtin {
        name: "air_temperature",
        descriptors: &["012101", "012004"],
        unit: "K",
        label: "Air temperature",
        observed_property: "air_temperature",
        period_hours: None,
    },
    Builtin {
        name: "dew_point_temperature",
        descriptors: &["012103", "012006"],
        unit: "K",
        label: "Dew point temperature",
        observed_property: "dew_point_temperature",
        period_hours: None,
    },
    Builtin {
        name: "relative_humidity",
        descriptors: &["013003"],
        unit: "%",
        label: "Relative humidity",
        observed_property: "relative_humidity",
        period_hours: None,
    },
    Builtin {
        name: "pressure",
        descriptors: &["010004"],
        unit: "Pa",
        label: "Station pressure",
        observed_property: "air_pressure",
        period_hours: None,
    },
    Builtin {
        name: "pressure_msl",
        descriptors: &["010051"],
        unit: "Pa",
        label: "Pressure reduced to mean sea level",
        observed_property: "air_pressure_at_mean_sea_level",
        period_hours: None,
    },
    Builtin {
        name: "pressure_tendency_3h",
        descriptors: &["010061"],
        unit: "Pa",
        label: "3-hour pressure change",
        observed_property: "tendency_of_air_pressure",
        period_hours: None,
    },
    Builtin {
        name: "wind_direction",
        descriptors: &["011001"],
        unit: "deg",
        label: "Wind direction",
        observed_property: "wind_from_direction",
        period_hours: None,
    },
    Builtin {
        name: "wind_speed",
        descriptors: &["011002"],
        unit: "m s-1",
        label: "Wind speed",
        observed_property: "wind_speed",
        period_hours: None,
    },
    Builtin {
        name: "wind_gust",
        descriptors: &["011041"],
        unit: "m s-1",
        label: "Maximum wind gust speed",
        observed_property: "wind_speed_of_gust",
        period_hours: None,
    },
    Builtin {
        name: "precipitation_1h",
        descriptors: &["013011"],
        unit: "kg m-2",
        label: "Precipitation (1 h)",
        observed_property: "precipitation_amount",
        period_hours: Some(1.0),
    },
    Builtin {
        name: "precipitation_3h",
        descriptors: &["013011"],
        unit: "kg m-2",
        label: "Precipitation (3 h)",
        observed_property: "precipitation_amount",
        period_hours: Some(3.0),
    },
    Builtin {
        name: "precipitation_6h",
        descriptors: &["013011"],
        unit: "kg m-2",
        label: "Precipitation (6 h)",
        observed_property: "precipitation_amount",
        period_hours: Some(6.0),
    },
    Builtin {
        name: "precipitation_12h",
        descriptors: &["013011"],
        unit: "kg m-2",
        label: "Precipitation (12 h)",
        observed_property: "precipitation_amount",
        period_hours: Some(12.0),
    },
    Builtin {
        name: "precipitation_24h",
        descriptors: &["013011", "013023"],
        unit: "kg m-2",
        label: "Precipitation (24 h)",
        observed_property: "precipitation_amount",
        period_hours: Some(24.0),
    },
    Builtin {
        name: "air_temperature_max_12h",
        descriptors: &["012111"],
        unit: "K",
        label: "Maximum air temperature (12 h)",
        observed_property: "air_temperature",
        period_hours: Some(12.0),
    },
    Builtin {
        name: "air_temperature_min_12h",
        descriptors: &["012112"],
        unit: "K",
        label: "Minimum air temperature (12 h)",
        observed_property: "air_temperature",
        period_hours: Some(12.0),
    },
    Builtin {
        name: "air_temperature_max_24h",
        descriptors: &["012111"],
        unit: "K",
        label: "Maximum air temperature (24 h)",
        observed_property: "air_temperature",
        period_hours: Some(24.0),
    },
    Builtin {
        name: "air_temperature_min_24h",
        descriptors: &["012112"],
        unit: "K",
        label: "Minimum air temperature (24 h)",
        observed_property: "air_temperature",
        period_hours: Some(24.0),
    },
    Builtin {
        name: "visibility",
        descriptors: &["020001"],
        unit: "m",
        label: "Horizontal visibility",
        observed_property: "visibility_in_air",
        period_hours: None,
    },
    Builtin {
        name: "cloud_cover_total",
        descriptors: &["020010"],
        unit: "%",
        label: "Total cloud cover",
        observed_property: "cloud_area_fraction",
        period_hours: None,
    },
    Builtin {
        name: "present_weather",
        descriptors: &["020003"],
        unit: "code table 0 20 003",
        label: "Present weather",
        observed_property: "present_weather",
        period_hours: None,
    },
    Builtin {
        name: "snow_depth",
        descriptors: &["013013"],
        unit: "m",
        label: "Total snow depth",
        observed_property: "surface_snow_thickness",
        period_hours: None,
    },
];

/// The resolved column set of one collection.
#[derive(Debug, Clone)]
pub struct ParameterTable {
    pub params: Vec<ParamDef>,
    by_name: HashMap<String, usize>,
}

impl ParameterTable {
    /// Built-ins (unless disabled) merged with config overrides: a config
    /// entry with a built-in name replaces it in place, others append.
    pub fn build(builtin: bool, overrides: &[BufrParameterConfig]) -> Self {
        let mut params: Vec<ParamDef> = if builtin {
            BUILTIN
                .iter()
                .map(|b| ParamDef {
                    name: b.name.to_string(),
                    descriptors: b
                        .descriptors
                        .iter()
                        .filter_map(|c| xy_from_code(c))
                        .collect(),
                    unit: b.unit.to_string(),
                    label: b.label.to_string(),
                    observed_property: b.observed_property.to_string(),
                    period_hours: b.period_hours,
                })
                .collect()
        } else {
            Vec::new()
        };
        for o in overrides {
            let def = ParamDef {
                name: o.name.clone(),
                descriptors: o
                    .descriptors
                    .iter()
                    .filter_map(|c| xy_from_code(c))
                    .collect(),
                unit: o.unit.clone(),
                label: o.label.clone().unwrap_or_else(|| o.name.replace('_', " ")),
                observed_property: o
                    .observed_property
                    .clone()
                    .unwrap_or_else(|| o.name.clone()),
                period_hours: o.period_hours,
            };
            match params.iter().position(|p| p.name == def.name) {
                Some(i) => params[i] = def,
                None => params.push(def),
            }
        }
        let by_name = params
            .iter()
            .enumerate()
            .map(|(i, p)| (p.name.clone(), i))
            .collect();
        ParameterTable { params, by_name }
    }

    pub fn len(&self) -> usize {
        self.params.len()
    }

    pub fn is_empty(&self) -> bool {
        self.params.is_empty()
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.by_name.get(name).copied()
    }

    pub fn names(&self) -> Vec<String> {
        self.params.iter().map(|p| p.name.clone()).collect()
    }

    pub fn descriptions(&self) -> HashMap<String, ParameterDescription> {
        self.params
            .iter()
            .map(|p| {
                (
                    p.name.clone(),
                    ParameterDescription {
                        label: p.label.clone(),
                        unit: p.unit.clone(),
                        observed_property: p.observed_property.clone(),
                    },
                )
            })
            .collect()
    }

    /// Extract one dense row (`NaN` = missing) from a report.
    pub fn row(&self, report: &ObsReport) -> Box<[f32]> {
        self.params
            .iter()
            .map(|p| {
                p.descriptors
                    .iter()
                    .find_map(|&xy| report.value(xy, p.period_hours))
                    .map(|v| v as f32)
                    .unwrap_or(f32::NAN)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_names_are_unique_and_descriptors_parse() {
        let t = ParameterTable::build(true, &[]);
        let mut names = t.names();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), t.len());
        assert!(t.params.iter().all(|p| !p.descriptors.is_empty()));
        assert_eq!(t.index_of("air_temperature"), Some(0));
    }

    #[test]
    fn overrides_replace_by_name_and_append() {
        let o = vec![
            BufrParameterConfig {
                name: "air_temperature".into(),
                descriptors: vec!["012004".into()],
                unit: "K".into(),
                label: Some("T".into()),
                period_hours: None,
                observed_property: None,
            },
            BufrParameterConfig {
                name: "rain_2h".into(),
                descriptors: vec!["013011".into()],
                unit: "kg m-2".into(),
                label: None,
                period_hours: Some(2.0),
                observed_property: None,
            },
        ];
        let t = ParameterTable::build(true, &o);
        assert_eq!(t.params[0].label, "T");
        assert_eq!(t.params[0].descriptors.len(), 1);
        let last = t.params.last().unwrap();
        assert_eq!(last.name, "rain_2h");
        assert_eq!(last.label, "rain 2h");
        assert_eq!(last.observed_property, "rain_2h");
        assert_eq!(t.index_of("rain_2h"), Some(t.len() - 1));
        let only = ParameterTable::build(false, &o);
        assert_eq!(only.len(), 2);
    }
}
