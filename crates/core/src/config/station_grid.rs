//! The per-parameter and per-station parts of a station grid's config
//! (#1206): `parameters` entries as a name or a table with corrections, and
//! the `exclude_where` station filter. The engine (`engine-station-grid`)
//! checks the values when it is built; a bad value fails the collection.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;

/// One entry of `[station_grid] parameters`: the source parameter's name,
/// or a table naming it with its own corrections,
/// `{ name = "t2m", lapse_rate = -6.5, land_mask = false }`.
#[derive(Debug, Clone, PartialEq)]
pub enum StationGridParameter {
    Name(String),
    Options(StationGridParameterOptions),
}

/// The table form of a [`StationGridParameter`].
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StationGridParameterOptions {
    /// The source parameter's name.
    pub name: String,
    /// Temperature change with height in K/km (−6.5 is the standard
    /// atmosphere's), applied with the `elevation_source` terrain. Unset:
    /// −6.5 where the source's own metadata identifies air temperature,
    /// else no correction. `0` turns the correction off.
    #[serde(default)]
    pub lapse_rate: Option<f64>,
    /// Whether cells outside the `land_mask_source` mask are nodata for this
    /// parameter (default `true` when the collection has a mask).
    #[serde(default)]
    pub land_mask: Option<bool>,
}

impl StationGridParameter {
    /// The source parameter's name.
    pub fn name(&self) -> &str {
        match self {
            Self::Name(name) => name,
            Self::Options(options) => &options.name,
        }
    }

    /// The configured lapse rate in K/km, if any.
    pub fn lapse_rate(&self) -> Option<f64> {
        match self {
            Self::Name(_) => None,
            Self::Options(options) => options.lapse_rate,
        }
    }

    /// The configured mask opt-in or opt-out, if any.
    pub fn land_mask(&self) -> Option<bool> {
        match self {
            Self::Name(_) => None,
            Self::Options(options) => options.land_mask,
        }
    }
}

impl From<&str> for StationGridParameter {
    fn from(name: &str) -> Self {
        Self::Name(name.to_string())
    }
}

impl From<String> for StationGridParameter {
    fn from(name: String) -> Self {
        Self::Name(name)
    }
}

/// A string or a table, keeping the table's own error for an unknown or
/// mistyped key (an untagged enum would only say that nothing matched).
impl<'de> Deserialize<'de> for StationGridParameter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ParameterVisitor;

        impl<'de> Visitor<'de> for ParameterVisitor {
            type Value = StationGridParameter;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(
                    "a parameter name or a table { name = \"…\", lapse_rate = …, land_mask = … }",
                )
            }

            fn visit_str<E: de::Error>(self, name: &str) -> Result<Self::Value, E> {
                Ok(StationGridParameter::Name(name.to_string()))
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                StationGridParameterOptions::deserialize(de::value::MapAccessDeserializer::new(map))
                    .map(StationGridParameter::Options)
            }
        }

        deserializer.deserialize_any(ParameterVisitor)
    }
}

/// A station property value `exclude_where` compares with, as TOML writes
/// it.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum StationPropertyValue {
    Bool(bool),
    Integer(i64),
    Float(f64),
    Text(String),
}

impl StationPropertyValue {
    /// The value as the shared Features property matcher
    /// (`ds_core::feature::matches_property_values`) reads a filter: `true`,
    /// `42`, `1.5`, or the text.
    pub fn filter_text(&self) -> String {
        match self {
            Self::Bool(b) => b.to_string(),
            Self::Integer(n) => n.to_string(),
            Self::Float(n) => n.to_string(),
            Self::Text(s) => s.clone(),
        }
    }
}

/// One value or a list of them: a property matches any.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
enum OneOrMany {
    One(StationPropertyValue),
    Many(Vec<StationPropertyValue>),
}

/// One `exclude_where` rule: every property it names matches one of its
/// values.
pub type StationExclusionRule = BTreeMap<String, Vec<StationPropertyValue>>;

/// `[station_grid] exclude_where`: the stations of the source's station
/// inventory left out of every analysis. A table is one rule, a list of
/// tables several; a station matching any rule is excluded. A rule matches
/// when each property it names equals one of its values, as the Features
/// `/items` property filter compares them:
///
/// ```toml
/// exclude_where = { exclude_from_gridding = true }
/// exclude_where = [{ network = ["SHIP", "BUOY"] }, { name = "Test site" }]
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StationExclusions(pub Vec<StationExclusionRule>);

impl StationExclusions {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<'de> Deserialize<'de> for StationExclusions {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ExclusionsVisitor;

        fn rule<'de, A: MapAccess<'de>>(mut map: A) -> Result<StationExclusionRule, A::Error> {
            let mut rule = StationExclusionRule::new();
            while let Some((property, values)) = map.next_entry::<String, OneOrMany>()? {
                let values = match values {
                    OneOrMany::One(value) => vec![value],
                    OneOrMany::Many(values) => values,
                };
                if rule.insert(property.clone(), values).is_some() {
                    return Err(de::Error::custom(format!(
                        "exclude_where names property '{property}' twice"
                    )));
                }
            }
            Ok(rule)
        }

        struct RuleSeed;

        impl<'de> de::DeserializeSeed<'de> for RuleSeed {
            type Value = StationExclusionRule;

            fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
                struct RuleVisitor;
                impl<'de> Visitor<'de> for RuleVisitor {
                    type Value = StationExclusionRule;
                    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                        f.write_str("a table of station properties and values")
                    }
                    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                        rule(map)
                    }
                }
                d.deserialize_map(RuleVisitor)
            }
        }

        impl<'de> Visitor<'de> for ExclusionsVisitor {
            type Value = StationExclusions;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a table of station properties and values, or a list of such tables")
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                Ok(StationExclusions(vec![rule(map)?]))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut rules = Vec::new();
                while let Some(rule) = seq.next_element_seed(RuleSeed)? {
                    rules.push(rule);
                }
                Ok(StationExclusions(rules))
            }
        }

        deserializer.deserialize_any(ExclusionsVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct Holder {
        #[serde(default)]
        parameters: Vec<StationGridParameter>,
        #[serde(default)]
        exclude_where: StationExclusions,
    }

    fn parse(toml_text: &str) -> Result<Holder, String> {
        toml::from_str(toml_text).map_err(|e| e.to_string())
    }

    #[test]
    fn parameters_are_names_or_tables() {
        let h = parse(
            "parameters = [\"t2m\", { name = \"td\", lapse_rate = -2.0 }, \
             { name = \"ws\", land_mask = false }]",
        )
        .unwrap();
        assert_eq!(h.parameters.len(), 3);
        assert_eq!(h.parameters[0], StationGridParameter::from("t2m"));
        assert_eq!(
            (h.parameters[1].name(), h.parameters[1].lapse_rate()),
            ("td", Some(-2.0))
        );
        assert_eq!(
            (h.parameters[2].name(), h.parameters[2].land_mask()),
            ("ws", Some(false))
        );
        assert_eq!(h.parameters[0].lapse_rate(), None);
        // The table's own error names the unknown key.
        let typo = parse("parameters = [{ name = \"t2m\", lapse = -6.5 }]").unwrap_err();
        assert!(typo.contains("unknown field `lapse`"), "{typo}");
        let missing = parse("parameters = [{ lapse_rate = -6.5 }]").unwrap_err();
        assert!(missing.contains("missing field `name`"), "{missing}");
        let wrong = parse("parameters = [42]").unwrap_err();
        assert!(wrong.contains("a parameter name or a table"), "{wrong}");
    }

    #[test]
    fn exclusions_are_a_table_or_a_list_of_tables() {
        let one = parse("exclude_where = { exclude_from_gridding = true }").unwrap();
        assert_eq!(
            one.exclude_where.0,
            vec![StationExclusionRule::from([(
                "exclude_from_gridding".to_string(),
                vec![StationPropertyValue::Bool(true)]
            )])]
        );
        let many = parse(
            "exclude_where = [{ network = [\"SHIP\", \"BUOY\"], height = 2 }, \
             { name = \"Test\" }, { level = 1.5 }]",
        )
        .unwrap();
        assert_eq!(many.exclude_where.0.len(), 3);
        assert_eq!(
            many.exclude_where.0[0]["network"]
                .iter()
                .map(StationPropertyValue::filter_text)
                .collect::<Vec<_>>(),
            ["SHIP", "BUOY"]
        );
        assert_eq!(many.exclude_where.0[0]["height"][0].filter_text(), "2");
        assert_eq!(many.exclude_where.0[2]["level"][0].filter_text(), "1.5");
        assert!(parse("").unwrap().exclude_where.is_empty());
        let bad = parse("exclude_where = 3").unwrap_err();
        assert!(bad.contains("a table of station properties"), "{bad}");
        let nested = parse("exclude_where = { a = { b = 1 } }").unwrap_err();
        assert!(!nested.is_empty());
    }
}
