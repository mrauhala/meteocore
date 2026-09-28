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
    // Satellite IR window brightness temperature and cloud top
    // temperature (#819): 180–330 K spans the coldest storm tops to the
    // warmest land, far wider than air temperature's range. Before the
    // temperature rule, which would otherwise claim both titles. The palette
    // stops are in kelvin, so the rule requires a K unit.
    DefaultRule {
        names: &[],
        contains: &["brightnesstemperature", "cloudtoptemperature"],
        palette: "ir_bt_enhanced",
        unit_ranges: &[(&["k", "kelvin"], 180.0, 330.0)],
        fallback_range: None,
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
    // Snowfall water equivalent (ECMWF `sf`) — the precipitation scale, but
    // unit-gated: snowfall is also published in metres, which 0..50 would
    // render as no snow at all.
    DefaultRule {
        names: &["sf"],
        contains: &["snowfall"],
        palette: "precipitation",
        unit_ranges: &[(&["mm", "kgm2"], 0.0, 50.0)],
        fallback_range: None,
    },
    // Signed wind components (u/v) — diverging about zero, on the wind-speed
    // rule's scale. Before it, so a "u-component of wind gust" title stays
    // signed.
    DefaultRule {
        names: &[
            "u", "v", "10u", "10v", "100u", "100v", "u10", "v10", "u100", "v100", "ugrd", "vgrd",
        ],
        contains: &[
            "componentofwind",
            "windcomponent",
            "eastwardwind",
            "northwardwind",
        ],
        palette: "diverging",
        unit_ranges: &[
            (&["ms", "ms1", "mps"], -40.0, 40.0),
            (&["kt", "kn", "knots"], -80.0, 80.0),
        ],
        fallback_range: None,
    },
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
    // Vertical velocity — diverging about zero. Pressure (omega, Pa s-1,
    // negative = ascent) and geometric (m s-1) velocities differ in unit and
    // scale, so the rule is unit-gated. Before the pressure rule, which would
    // otherwise claim the "Vertical velocity (pressure)" title.
    DefaultRule {
        names: &["w", "omega", "vvel", "wz", "dzdt"],
        contains: &["verticalvelocity"],
        palette: "diverging",
        unit_ranges: &[(&["pas1", "pas"], -2.0, 2.0), (&["ms", "ms1"], -1.0, 1.0)],
        fallback_range: None,
    },
    // Vorticity (relative or absolute), s-1 — diverging about zero.
    // Potential vorticity carries another unit and does not match.
    DefaultRule {
        names: &["vo", "relv", "absv"],
        contains: &["vorticity"],
        palette: "diverging",
        unit_ranges: &[(&["s1", "1s"], -2e-4, 2e-4)],
        fallback_range: None,
    },
    // Divergence, s-1 — diverging about zero; convergence is negative.
    DefaultRule {
        names: &["d", "reld"],
        contains: &["divergence"],
        palette: "diverging",
        unit_ranges: &[(&["s1", "1s"], -1e-4, 1e-4)],
        fallback_range: None,
    },
    // Geopotential / geopotential height. The matcher never sees the level,
    // so one range serves every pressure level: 0–21 000 gpm spans 1000 hPa
    // up to 50 hPa (~20.6 km), and 206 000 m2 s-2 is the same height as
    // geopotential. Names only: a "Geopotential height anomaly" title is
    // signed and must not match.
    DefaultRule {
        names: &[
            "z",
            "gh",
            "hgt",
            "fi",
            "zg",
            "geopotential",
            "geopotentialheight",
        ],
        contains: &[],
        palette: "viridis",
        unit_ranges: &[(&["gpm", "m"], 0.0, 21_000.0), (&["m2s2"], 0.0, 206_000.0)],
        fallback_range: None,
    },
    // Surface pressure: elevated terrain reaches ~500 hPa, so it needs a far
    // wider range than MSLP, and a normalized palette — the MSLP palette's
    // stops are 950–1050 hPa, which would paint every plateau one colour.
    // Before the MSLP rule, which the "Pressure (surface)" title also matches.
    DefaultRule {
        names: &["sp"],
        contains: &["surfacepressure", "pressuresurface", "surfaceairpressure"],
        palette: "viridis",
        unit_ranges: &[
            (&["pa"], 50_000.0, 105_000.0),
            (&["hpa", "mbar", "mb"], 500.0, 1050.0),
        ],
        fallback_range: None,
    },
    // Pressure / MSLP — unit-gated: Pa vs hPa ranges differ 100×.
    DefaultRule {
        names: &["msl", "mslp", "pres", "slp", "prmsl"],
        contains: &["pressure", "mslp"],
        palette: "pressure",
        unit_ranges: &[
            (&["pa"], 95000.0, 105000.0),
            (&["hpa", "mbar", "mb"], 950.0, 1050.0),
        ],
        fallback_range: None,
    },
    // Specific humidity / humidity mixing ratio, a mass fraction — never the
    // relative-humidity percent range. Unit-gated, and the RH rule below
    // matches only relative humidity, so an unknown unit gets no default.
    DefaultRule {
        names: &["q", "spfh"],
        contains: &["specifichumidity", "humiditymixingratio"],
        palette: "viridis",
        unit_ranges: &[
            (&["kgkg1", "kgkg", "1"], 0.0, 0.02),
            (&["gkg1", "gkg"], 0.0, 20.0),
        ],
        fallback_range: None,
    },
    // Total column water (vapour), mm = kg m-2.
    DefaultRule {
        names: &["tcwv", "tcw"],
        contains: &["totalcolumnwater", "columnintegratedwatervapour"],
        palette: "viridis",
        unit_ranges: &[(&["mm", "kgm2"], 0.0, 70.0)],
        fallback_range: None,
    },
    // Relative humidity. `contains` names relative humidity explicitly:
    // "Specific humidity" must not reach the percent range. A bare
    // `Humidity` short name (FMI QueryData, no unit metadata) still matches.
    DefaultRule {
        names: &["rh", "r", "2r", "r2", "humidity"],
        contains: &["relativehumidity"],
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

    /// Satellite brightness temperature and cloud top temperature get the
    /// IR palette over 180–330 K — not the air-temperature rule, whose
    /// titles they also contain.
    #[test]
    fn satellite_ir_titles_get_the_ir_palette() {
        let d = builtin();
        for title in ["IR 10.3 µm brightness temperature", "Cloud top temperature"] {
            let m = d.match_default("x", title, Some("K")).unwrap();
            assert_eq!(m.palette, "ir_bt_enhanced", "{title}");
            assert_eq!(m.range, Some((180.0, 330.0)), "{title}");
        }
        // The IR stops are kelvin: in °C the title falls through to the
        // air-temperature rule, and with no unit nothing is guessed.
        let celsius = d
            .match_default("x", "Cloud top temperature", Some("°C"))
            .unwrap();
        assert_eq!(celsius.palette, "temperature");
        assert_eq!(d.match_default("x", "Cloud top temperature", None), None);
        // Air temperature is unchanged.
        let air = d
            .match_default("param42", "2 metre temperature", Some("K"))
            .unwrap();
        assert_eq!(air.palette, "temperature");
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

    /// #763: GRIB fields that had no rule or the wrong one, with the names,
    /// titles and display units engine-grib publishes (plus the wgrib2 and
    /// CF spellings of the same fields).
    #[test]
    fn grib_fields_get_field_specific_defaults() {
        let d = builtin();
        let wind = ("diverging", (-40.0, 40.0));
        let height = ("viridis", (0.0, 21_000.0));
        for (name, title, unit, (palette, range)) in [
            ("u", "u-component of wind", "m s-1", wind),
            ("v", "v-component of wind", "m s-1", wind),
            (
                "10u",
                "u-component of wind (10 m above ground)",
                "m s-1",
                wind,
            ),
            (
                "10v",
                "v-component of wind (10 m above ground)",
                "m s-1",
                wind,
            ),
            (
                "UGRD",
                "u-component of wind (10 m above ground)",
                "m/s",
                wind,
            ),
            ("u10", "10 metre U wind component", "m s**-1", wind),
            ("10u", "", "kt", ("diverging", (-80.0, 80.0))),
            (
                "w",
                "Vertical velocity (pressure)",
                "Pa s-1",
                ("diverging", (-2.0, 2.0)),
            ),
            (
                "DZDT",
                "Vertical velocity (geometric)",
                "m s-1",
                ("diverging", (-1.0, 1.0)),
            ),
            ("z", "Geopotential", "gpm", height),
            ("gh", "Geopotential height", "gpm", height),
            ("HGT", "Geopotential height (500 hPa)", "gpm", height),
            (
                "z",
                "Geopotential",
                "m**2 s**-2",
                ("viridis", (0.0, 206_000.0)),
            ),
            (
                "q",
                "Specific humidity",
                "kg kg-1",
                ("viridis", (0.0, 0.02)),
            ),
            (
                "SPFH",
                "Specific humidity",
                "kg/kg",
                ("viridis", (0.0, 0.02)),
            ),
            ("q", "Specific humidity", "g kg-1", ("viridis", (0.0, 20.0))),
            (
                "sp",
                "Pressure (surface)",
                "hPa",
                ("viridis", (500.0, 1050.0)),
            ),
            (
                "PRES",
                "Pressure (surface)",
                "Pa",
                ("viridis", (50_000.0, 105_000.0)),
            ),
            (
                "vo",
                "Relative vorticity",
                "s-1",
                ("diverging", (-2e-4, 2e-4)),
            ),
            (
                "d",
                "Relative divergence",
                "s-1",
                ("diverging", (-1e-4, 1e-4)),
            ),
            (
                "tcwv",
                "Total column integrated water vapour (surface)",
                "mm",
                ("viridis", (0.0, 70.0)),
            ),
            (
                "sf",
                "Snowfall (water equivalent) (surface)",
                "mm",
                ("precipitation", (0.0, 50.0)),
            ),
        ] {
            let m = d
                .match_default(name, title, Some(unit))
                .unwrap_or_else(|| panic!("no default for {name} / {title} / {unit}"));
            assert_eq!(m.palette, palette, "{name} / {title} / {unit}");
            assert_eq!(m.range, Some(range), "{name} / {title} / {unit}");
        }
    }

    /// The #763 rules never guess a unit: with none, or one they do not
    /// know, they do not apply — and nothing falls through to a neighbour
    /// with another meaning (specific humidity to the RH percent range,
    /// surface pressure or omega to the MSLP range).
    #[test]
    fn grib_rules_are_unit_gated_without_fall_through() {
        let d = builtin();
        for (name, title) in [
            ("u", "u-component of wind"),
            ("w", "Vertical velocity (pressure)"),
            ("gh", "Geopotential height"),
            ("q", "Specific humidity"),
            ("x", "Humidity mixing ratio"),
            ("sp", "Pressure (surface)"),
            ("vo", "Relative vorticity"),
            ("d", "Relative divergence"),
            ("tcwv", "Total column integrated water vapour"),
            ("sf", "Snowfall (water equivalent)"),
            // engine-grib's label and unit before its metadata is known.
            ("tcw", "tcw"),
        ] {
            assert_eq!(d.match_default(name, title, None), None, "{name}");
            assert_eq!(
                d.match_default(name, title, Some("furlong")),
                None,
                "{name}"
            );
        }
        // Snowfall in metres is not millimetres.
        assert_eq!(d.match_default("sf", "Snowfall", Some("m")), None);
        // Potential vorticity has its own unit; an anomaly is signed.
        assert_eq!(
            d.match_default("pv", "Potential vorticity", Some("K m2 kg-1 s-1")),
            None
        );
        assert_eq!(
            d.match_default("5WAVA", "Geopotential height anomaly", Some("gpm")),
            None
        );
    }

    /// Relative humidity keeps its percent and fraction ranges, and the
    /// unit-less FMI QueryData `Humidity` its percent fallback; MSLP keeps
    /// 950–1050 hPa.
    #[test]
    fn relative_humidity_and_mslp_keep_their_defaults() {
        let d = builtin();
        let hit = |name, title, unit| d.match_default(name, title, unit).unwrap();
        for (name, title, unit, palette, range) in [
            (
                "r",
                "Relative humidity",
                Some("%"),
                "humidity",
                (0.0, 100.0),
            ),
            (
                "2r",
                "2 metre relative humidity",
                Some("1"),
                "humidity",
                (0.0, 1.0),
            ),
            ("Humidity", "Humidity", None, "humidity", (0.0, 100.0)),
            (
                "msl",
                "Pressure (mean sea level)",
                Some("hPa"),
                "pressure",
                (950.0, 1050.0),
            ),
            (
                "PRMSL",
                "Pressure reduced to MSL",
                Some("Pa"),
                "pressure",
                (95_000.0, 105_000.0),
            ),
        ] {
            let m = hit(name, title, unit);
            assert_eq!(m.palette, palette, "{name}");
            assert_eq!(m.range, Some(range), "{name}");
        }
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
