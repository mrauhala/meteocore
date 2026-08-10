//! Built-in per-parameter default styles (#320).
//!
//! When a multi-parameter collection has no explicit style for a parameter,
//! the resolver consults this table (config `[[parameter_defaults]]` rules
//! first, then the embedded rules) so temperature renders as temperature and
//! pressure as pressure instead of everything getting one collection-wide
//! colormap — or viridis 0..1.
//!
//! Matching is **display styling only** — never unit conversion or semantic
//! interpretation. It is best-effort by normalized parameter name/title plus
//! a unit hint, and fails soft: no match ⇒ the existing fallback chain.
//! Unit-gated rules (temperature, pressure) NEVER guess the unit: with no
//! matching unit alias and no fallback range, the rule does not apply.

/// One built-in matching rule. All name/`contains` matching happens on
/// [`normalize`]d strings (lowercase, alphanumeric only).
struct DefaultRule {
    /// Exact (normalized) short-name matches, e.g. `t2m`, `dbzh`.
    names: &'static [&'static str],
    /// Substring (normalized) matches against the short name AND title.
    contains: &'static [&'static str],
    /// Palette name (must exist in the builtin table).
    palette: &'static str,
    /// Unit-alias groups → value range, checked against the unit hint.
    unit_ranges: &'static [(&'static [&'static str], f64, f64)],
    /// Range when no unit alias matched. `None` with a non-empty
    /// `unit_ranges` means the rule REQUIRES a unit match (never guess
    /// K vs °C); `None` with empty `unit_ranges` means the palette's own
    /// stop range applies (data-valued palettes like radar_dbz).
    fallback_range: Option<(f64, f64)>,
}

/// First match wins — order the specific before the generic.
static RULES: &[DefaultRule] = &[
    // Radar reflectivity — data-valued palette (stops carry dBZ).
    // `contains` deliberately matches only "dbz", NOT "reflectivity": polar
    // moment titles like "Differential reflectivity" (ZDR) must not be
    // painted with the dBZ ramp. Plain `parameter = "reflectivity"`
    // collections hit the exact-name list.
    DefaultRule {
        names: &["dbzh", "dbzv", "th", "tv", "dbz", "reflectivity"],
        contains: &["dbz"],
        palette: "radar_dbz",
        unit_ranges: &[],
        fallback_range: None,
    },
    // Doppler radial velocity — diverging about zero.
    DefaultRule {
        names: &["vradh", "vradv", "vrad"],
        contains: &["radialvelocity"],
        palette: "radial_velocity",
        unit_ranges: &[],
        fallback_range: Some((-48.0, 48.0)),
    },
    // Temperature / dew point — unit-gated: NEVER guess K vs °C.
    DefaultRule {
        names: &["t", "2t", "t2m", "tmp", "tt", "td", "2d", "d2m", "skt"],
        contains: &["temperature", "dewpoint"],
        palette: "temperature",
        unit_ranges: &[
            (&["k", "kelvin"], 233.15, 323.15),
            (&["c", "degc", "celsius", "cel"], -40.0, 50.0),
        ],
        fallback_range: None,
    },
    // Precipitation rate / intensity.
    DefaultRule {
        names: &["prate", "rr", "rri", "prr"],
        contains: &["precipitationrate", "rainrate", "rainintensity"],
        palette: "precipitation_rate",
        unit_ranges: &[(&["mmh", "mmhr", "mmh1", "kgm2s1"], 0.0, 30.0)],
        fallback_range: Some((0.0, 30.0)),
    },
    // Precipitation amount / accumulation.
    DefaultRule {
        names: &["tp", "apcp", "rr1h", "rr24h", "acrr"],
        contains: &["precipitation", "precip", "rainfall", "accum"],
        palette: "precipitation",
        unit_ranges: &[],
        fallback_range: Some((0.0, 50.0)),
    },
    // Wind speed / gust.
    DefaultRule {
        names: &["ws", "ff", "si10", "10si", "gust", "fg", "wgust"],
        contains: &["windspeed", "windgust"],
        palette: "wind_speed",
        unit_ranges: &[
            (&["ms", "ms1", "mps"], 0.0, 40.0),
            (&["kt", "kn", "knots"], 0.0, 80.0),
        ],
        fallback_range: Some((0.0, 40.0)),
    },
    // Pressure / MSLP — unit-gated: Pa vs hPa ranges differ 100×.
    DefaultRule {
        names: &["msl", "mslp", "pres", "slp", "prmsl", "sp"],
        contains: &["pressure", "mslp"],
        palette: "pressure",
        unit_ranges: &[
            (&["pa"], 95000.0, 105000.0),
            (&["hpa", "mbar", "mb"], 950.0, 1050.0),
        ],
        fallback_range: None,
    },
    // Relative humidity.
    DefaultRule {
        names: &["rh", "r", "2r", "r2"],
        contains: &["humidity"],
        palette: "humidity",
        unit_ranges: &[(&["", "percent", "pct"], 0.0, 100.0), (&["1"], 0.0, 1.0)],
        fallback_range: Some((0.0, 100.0)),
    },
    // Cloud cover.
    DefaultRule {
        names: &["tcc", "cc", "n", "nt", "clct"],
        contains: &["cloudcover", "cloudiness", "totalcloud"],
        palette: "cloud_cover",
        unit_ranges: &[
            (&["", "percent", "pct"], 0.0, 100.0),
            (&["1", "01"], 0.0, 1.0),
        ],
        fallback_range: Some((0.0, 100.0)),
    },
    // CAPE.
    DefaultRule {
        names: &["cape"],
        contains: &["cape"],
        palette: "viridis",
        unit_ranges: &[],
        fallback_range: Some((0.0, 4000.0)),
    },
];

/// A matched default: the palette name plus an explicit range when the rule
/// defines one (`None` ⇒ the palette's own stop range applies).
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedDefault {
    pub palette: String,
    pub range: Option<(f64, f64)>,
}

/// One user-configured override rule (`[[parameter_defaults]]`), checked
/// before the embedded table. Owned strings, same semantics as the
/// embedded rules.
#[derive(Clone, Debug)]
pub struct DefaultOverride {
    pub names: Vec<String>,
    pub contains: Vec<String>,
    pub palette: String,
    pub unit_ranges: Vec<(Vec<String>, f64, f64)>,
    pub fallback_range: Option<(f64, f64)>,
}

/// The defaults matcher: config overrides first, then the embedded table.
#[derive(Clone, Debug, Default)]
pub struct ParameterDefaults {
    overrides: Vec<DefaultOverride>,
}

impl ParameterDefaults {
    pub fn with_overrides(overrides: Vec<DefaultOverride>) -> Self {
        Self { overrides }
    }

    /// Match a parameter (short name + human title + unit hint) to a
    /// default style. First match wins; config overrides run first.
    pub fn match_default(
        &self,
        short_name: &str,
        title: &str,
        unit: Option<&str>,
    ) -> Option<ResolvedDefault> {
        let name_n = normalize(short_name);
        let title_n = normalize(title);
        let unit_n = unit.map(normalize);

        for rule in &self.overrides {
            let name_hit = rule.names.iter().any(|n| normalize(n) == name_n)
                || rule
                    .contains
                    .iter()
                    .map(|c| normalize(c))
                    .any(|c| !c.is_empty() && (name_n.contains(&c) || title_n.contains(&c)));
            if !name_hit {
                continue;
            }
            let range = match resolve_range_owned(rule, unit_n.as_deref()) {
                Ok(r) => r,
                Err(()) => continue, // unit-gated rule, no unit match
            };
            return Some(ResolvedDefault {
                palette: rule.palette.clone(),
                range,
            });
        }

        for rule in RULES {
            let name_hit = rule.names.contains(&name_n.as_str())
                || rule
                    .contains
                    .iter()
                    .any(|c| name_n.contains(c) || title_n.contains(c));
            if !name_hit {
                continue;
            }
            let range = match resolve_range(rule, unit_n.as_deref()) {
                Ok(r) => r,
                Err(()) => continue, // unit-gated rule, no unit match
            };
            return Some(ResolvedDefault {
                palette: rule.palette.to_string(),
                range,
            });
        }
        None
    }
}

/// `Ok(Some(range))` — explicit range; `Ok(None)` — palette stop range;
/// `Err(())` — unit-gated rule whose gate failed (rule does not apply).
fn resolve_range(rule: &DefaultRule, unit: Option<&str>) -> Result<Option<(f64, f64)>, ()> {
    if rule.unit_ranges.is_empty() {
        return Ok(rule.fallback_range);
    }
    if let Some(u) = unit {
        for (aliases, min, max) in rule.unit_ranges {
            if aliases.contains(&u) {
                return Ok(Some((*min, *max)));
            }
        }
    }
    match rule.fallback_range {
        Some(r) => Ok(Some(r)),
        None => Err(()),
    }
}

fn resolve_range_owned(
    rule: &DefaultOverride,
    unit: Option<&str>,
) -> Result<Option<(f64, f64)>, ()> {
    if rule.unit_ranges.is_empty() {
        return Ok(rule.fallback_range);
    }
    if let Some(u) = unit {
        for (aliases, min, max) in &rule.unit_ranges {
            if aliases.iter().any(|a| normalize(a) == u) {
                return Ok(Some((*min, *max)));
            }
        }
    }
    match rule.fallback_range {
        Some(r) => Ok(Some(r)),
        None => Err(()),
    }
}

/// Lowercase alphanumerics only: `"10 m wind speed"` → `"10mwindspeed"`,
/// `"°C"` → `"c"`, `"m s**-1"` → `"ms1"`.
pub fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::builtin_palette;

    fn builtin() -> ParameterDefaults {
        ParameterDefaults::default()
    }

    #[test]
    fn every_rule_palette_exists() {
        for rule in RULES {
            assert!(
                builtin_palette(rule.palette).is_some(),
                "rule palette '{}' missing from builtin table",
                rule.palette
            );
        }
    }

    #[test]
    fn name_variants_match() {
        let d = builtin();
        for (name, unit, palette) in [
            ("t2m", Some("K"), "temperature"),
            ("2t", Some("C"), "temperature"),
            ("TMP", Some("K"), "temperature"),
            ("DBZH", None, "radar_dbz"),
            ("dbz", None, "radar_dbz"),
            ("VRADH", None, "radial_velocity"),
            ("msl", Some("Pa"), "pressure"),
            ("mslp", Some("hPa"), "pressure"),
            ("rh", Some("%"), "humidity"),
            ("tcc", Some("%"), "cloud_cover"),
            ("ws", Some("m s**-1"), "wind_speed"),
            ("cape", Some("J kg**-1"), "viridis"),
        ] {
            let m = d
                .match_default(name, "", unit)
                .unwrap_or_else(|| panic!("no default for {name}"));
            assert_eq!(m.palette, palette, "{name}");
        }
    }

    #[test]
    fn unit_gates_are_strict() {
        let d = builtin();
        // Temperature with K → Kelvin range.
        let k = d.match_default("t2m", "", Some("K")).unwrap();
        assert_eq!(k.range, Some((233.15, 323.15)));
        // …with °C → Celsius range (degree sign normalized away).
        let c = d.match_default("t2m", "", Some("°C")).unwrap();
        assert_eq!(c.range, Some((-40.0, 50.0)));
        // …with NO unit → rule refuses (never guess K vs C).
        assert_eq!(d.match_default("t2m", "", None), None);
        assert_eq!(d.match_default("t2m", "", Some("weird")), None);
        // Pressure likewise.
        assert_eq!(d.match_default("msl", "", None), None);
        // Pa vs hPa ranges differ 100×.
        assert_eq!(
            d.match_default("msl", "", Some("Pa")).unwrap().range,
            Some((95000.0, 105000.0))
        );
        assert_eq!(
            d.match_default("msl", "", Some("hPa")).unwrap().range,
            Some((950.0, 1050.0))
        );
    }

    #[test]
    fn title_contains_matches() {
        let d = builtin();
        let m = d
            .match_default("param42", "2 metre temperature", Some("K"))
            .unwrap();
        assert_eq!(m.palette, "temperature");
        let m = d
            .match_default("x", "Total cloud cover", Some("%"))
            .unwrap();
        assert_eq!(m.palette, "cloud_cover");
    }

    #[test]
    fn data_valued_palette_uses_stop_range() {
        let d = builtin();
        let m = d.match_default("DBZH", "", None).unwrap();
        assert_eq!(m.range, None); // radar_dbz stops carry the range
                                   // Wind speed in knots doubles the range.
        assert_eq!(
            d.match_default("ws", "", Some("kt")).unwrap().range,
            Some((0.0, 80.0))
        );
    }

    #[test]
    fn no_match_returns_none() {
        let d = builtin();
        assert_eq!(
            d.match_default("ZDR", "Differential reflectivity z", None),
            None
        );
        assert_eq!(d.match_default("unknown", "", Some("K")), None);
    }

    #[test]
    fn overrides_run_before_embedded() {
        let d = ParameterDefaults::with_overrides(vec![DefaultOverride {
            names: vec!["DBZH".into()],
            contains: vec![],
            palette: "grayscale".into(),
            unit_ranges: vec![],
            fallback_range: Some((0.0, 60.0)),
        }]);
        let m = d.match_default("dbzh", "", None).unwrap();
        assert_eq!(m.palette, "grayscale");
        assert_eq!(m.range, Some((0.0, 60.0)));
        // Non-overridden names still hit the embedded table.
        assert_eq!(
            d.match_default("t2m", "", Some("K")).unwrap().palette,
            "temperature"
        );
    }

    #[test]
    fn normalize_examples() {
        assert_eq!(normalize("10 m wind speed"), "10mwindspeed");
        assert_eq!(normalize("°C"), "c");
        assert_eq!(normalize("m s**-1"), "ms1");
        assert_eq!(normalize("kg m-2 s-1"), "kgm2s1");
    }
}
