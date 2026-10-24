//! Built-in per-parameter default styles (#320).
//!
//! When a multi-parameter collection has no explicit style for a parameter,
//! the resolver consults this table (config `[[parameter_defaults]]` rules
//! first, then the embedded rules) so temperature renders as temperature and
//! pressure as pressure instead of everything getting one collection-wide
//! colormap — or viridis 0..1. A single-parameter collection's layer with
//! no palette configured consults it too, for the parameter its engine
//! draws (`StyleContext::base_layer_default`).
//!
//! Matching is **display styling only** — never unit conversion or semantic
//! interpretation. It is best-effort by normalized parameter name/title plus
//! a unit hint, and fails soft: no match ⇒ the existing fallback chain.
//! Unit-gated rules (temperature, pressure) NEVER guess the unit: with no
//! matching unit alias and no fallback range, the rule does not apply.
//!
//! Units are matched by spelling-independent key ([`unit_key`]): the unit's
//! `ds_core::units::canonical_unit` spelling, [`normalize`]d, with `%` read
//! as `percent`. So `kg/m^2`, `kg m**-2` and `kg m-2` are one unit (`kgm2`),
//! `m/s` and `m s-1` another (`ms1`), `degC` and `°C` a third (`c`); config
//! `[[parameter_defaults]]` unit ranges are keyed the same way, and also
//! still match by their plain [`normalize`]d spelling, as before #1067.

/// One built-in matching rule. All name/`contains` matching happens on
/// [`normalize`]d strings (lowercase, alphanumeric only).
struct DefaultRule {
    /// Exact (normalized) short-name matches, e.g. `t2m`, `dbzh`.
    names: &'static [&'static str],
    /// Substring (normalized) matches against the short name AND title.
    contains: &'static [&'static str],
    /// Palette name (must exist in the builtin table).
    palette: &'static str,
    /// Unit-key groups → value range, checked against the unit hint's
    /// [`unit_key`] (canonical spelling, normalized: `kgm2`, `ms1`, `c`).
    unit_ranges: &'static [(&'static [&'static str], f64, f64)],
    /// Range when no unit alias matched. `None` with a non-empty
    /// `unit_ranges` means the rule REQUIRES a unit match (never guess
    /// K vs °C); `None` with empty `unit_ranges` means the palette's own
    /// stop range applies (data-valued palettes like radar_dbz).
    fallback_range: Option<(f64, f64)>,
}

/// Unit keys of a fraction (CF's dimensionless `1`, GRIB's `(0 - 1)`).
const FRACTION: &[&str] = &["1", "01"];
/// Unit keys of a percentage ([`unit_key`] reads `%` as `percent`, so a
/// unit with no letters or digits, such as `-`, is not one).
const PERCENT: &[&str] = &["percent", "pct"];
const RH_NAMES: &[&str] = &["rh", "r", "2r", "r2", "humidity"];
const RH_CONTAINS: &[&str] = &["relativehumidity"];
const CLOUD_NAMES: &[&str] = &["tcc", "cc", "n", "nt", "clct"];
const CLOUD_CONTAINS: &[&str] = &["cloudcover", "cloudiness", "totalcloud"];
/// CF area fractions (#1067): `cloud_area_fraction`, its
/// `low_/medium_/high_type_…` and `convective_…` layers, `fog_area_fraction`.
const CF_CLOUD_CONTAINS: &[&str] = &["cloudareafraction", "fogareafraction"];

/// Geopotential height's one range for every pressure level, gpm: 1000 hPa
/// up to 50 hPa (~20.6 km).
const GEOPOTENTIAL_HEIGHT_MAX: f64 = 21_000.0;
/// The same height as geopotential, m² s⁻²: a level draws one colour as
/// `z` and as the `gh` derived from it (#948).
const GEOPOTENTIAL_MAX: f64 = GEOPOTENTIAL_HEIGHT_MAX * ds_core::geopotential::STANDARD_GRAVITY;

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
    // The lightning strike-age layer (engine-postgis events shape, #504):
    // the value is the strike's age in minutes, on the data-valued
    // `lightning_age` palette (0–60 min, the default one-hour window).
    // Unit-gated: a `lightning_age` in other units says nothing about age.
    DefaultRule {
        names: &["lightningage"],
        contains: &[],
        palette: "lightning_age",
        unit_ranges: &[(&["min"], 0.0, 60.0)],
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
    // Temperature / dew point — unit-gated: NEVER guess K vs °C. One range
    // for every level, −90…50 °C, so a colour means the same temperature
    // from the surface to the stratosphere. The kelvin range is the same
    // span, so the °C palette rescales onto it exactly. `dpt` is ECMWF's
    // and wgrib2's dew point, the name a derived one takes (#898).
    DefaultRule {
        names: &[
            "t", "2t", "t2m", "tmp", "tt", "td", "2d", "d2m", "dpt", "skt",
        ],
        contains: &["temperature", "dewpoint"],
        palette: "temperature",
        unit_ranges: &[
            (&["k", "kelvin"], 183.15, 323.15),
            (&["c", "degc", "celsius", "cel"], -90.0, 50.0),
        ],
        fallback_range: None,
    },
    // Precipitation rate / intensity, in mm/h. A rate stored in
    // kg m-2 s-1 is served in mm/h by every engine that converts units
    // (`ds_core::units::display_conversion` and engine-grib's twin, #1213),
    // so that unit has no range here: 0–30 kg m-2 s-1 would be 108 000 mm/h.
    DefaultRule {
        names: &["prate", "rr", "rri", "prr"],
        contains: &["precipitationrate", "rainrate", "rainintensity"],
        palette: "precipitation_rate",
        unit_ranges: &[(&["mmh", "mmhr", "mmh1", "mmhr1"], 0.0, 30.0)],
        fallback_range: Some((0.0, 30.0)),
    },
    // Precipitation accumulated from the run's start (#1067): MET Norway's
    // MEPS `precipitation_amount_acc`, and the `…fall_amount_acc` names of
    // the same pattern, grow through a 66-hour run far past the per-step
    // scale, so they get their own palette over 0–200 mm. Exact names: a
    // title saying "accumulated" says nothing about since when. Unit-gated
    // (kg m-2 = mm of water); before the per-step rules, which also claim
    // these names.
    DefaultRule {
        names: &[
            "precipitationamountacc",
            "rainfallamountacc",
            "snowfallamountacc",
            "graupelfallamountacc",
        ],
        contains: &[],
        palette: "precipitation_accumulation",
        unit_ranges: &[(&["kgm2", "mm"], 0.0, 200.0)],
        fallback_range: None,
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
    // Wind components on pressure levels (#1067 PR 8): MET Norway's MEPS
    // `x_wind_pl`/`y_wind_pl`. The matcher never sees the level, so one
    // range serves every level, and it must hold the jet: at 300–250 hPa the
    // jet stream's core commonly runs at 60–80 m/s over the Nordic domain
    // (the committed sample's 300 hPa x component reaches 72 m/s), which the
    // surface components' ±40 m/s would paint one colour. Twice the surface
    // range, symmetric so zero stays on the white midpoint. Exact names:
    // GRIB's level-view `u`/`v` keep the surface scale below.
    DefaultRule {
        names: &["xwindpl", "ywindpl"],
        contains: &[],
        palette: "diverging",
        unit_ranges: &[
            (&["ms", "ms1", "mps"], -80.0, 80.0),
            (&["kt", "kn", "knots"], -160.0, 160.0),
        ],
        fallback_range: None,
    },
    // The speed derived from those components (`wind_speed_pl`), on the
    // same 0–80 m/s scale. The wind-speed palette's stops end at 50 m/s, so
    // a faster jet core takes its last colour (#823: a range overlapping a
    // data-valued palette clips, never rescales). Before the generic speed
    // rule, whose `windspeed` would claim the name.
    DefaultRule {
        names: &["windspeedpl"],
        contains: &[],
        palette: "wind_speed",
        unit_ranges: &[
            (&["ms", "ms1", "mps"], 0.0, 80.0),
            (&["kt", "kn", "knots"], 0.0, 160.0),
        ],
        fallback_range: Some((0.0, 80.0)),
    },
    // Signed wind components (u/v) — diverging about zero, on the wind-speed
    // rule's scale. Before it, so a "u-component of wind gust" title stays
    // signed. CF's grid-relative `x_wind`/`y_wind` and MET Norway's
    // `x_wind_10m`/`y_wind_10m` (#1067) are exact names: a `contains`
    // "xwind" would claim every `max_wind…`. FMI newbase's `WindUMS` /
    // `WindVMS` (#1215) are QueryData's u and v, in m/s by their numbers.
    DefaultRule {
        names: &[
            "u", "v", "10u", "10v", "100u", "100v", "u10", "v10", "u100", "v100", "ugrd", "vgrd",
            "xwind", "ywind", "xwind10m", "ywind10m", "windums", "windvms",
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
    // Wind speed, native or derived from u/v (#897: `10si`, `100si`, `ws`,
    // wgrib2 `WIND`, FMI `WindSpeedMS`, CF stores' `…wind_speed…`; every
    // derived title starts "Wind speed"), and gusts. FMI newbase's
    // `HourlyMaximumGust` (#1215, m/s by its number) needs its exact name:
    // it holds no "windgust".
    DefaultRule {
        names: &[
            "ws",
            "ff",
            "si10",
            "10si",
            "si100",
            "100si",
            "wind",
            "gust",
            "fg",
            "wgust",
            "hourlymaximumgust",
        ],
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
    // up to 50 hPa (~20.6 km), and that height times g₀ (~205 940 m2 s-2)
    // as geopotential. Names only: a "Geopotential height anomaly" title is
    // signed and must not match. Geopotential is served in m² s⁻² (#948):
    // MEPS's `geopotential_pl` (#1067 PR 8) as stored, m^2/s^2, ECMWF's `z`
    // likewise; their derived heights (`geopotential_height_pl`, `gh`) are
    // in gpm.
    DefaultRule {
        names: &[
            "z",
            "gh",
            "gp",
            "hgt",
            "fi",
            "zg",
            "geopotential",
            "geopotentialheight",
            "geopotentialpl",
            "geopotentialheightpl",
        ],
        contains: &[],
        palette: "viridis",
        unit_ranges: &[
            (&["gpm", "m"], 0.0, GEOPOTENTIAL_HEIGHT_MAX),
            (&["m2s2"], 0.0, GEOPOTENTIAL_MAX),
        ],
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
    // Relative humidity as a fraction, in unit 1 as a source serves it
    // unconverted (engine-netcdf and engine-zarr serve CF's
    // `relative_humidity` in %, `ds_core::units::cf_display_conversion`,
    // so MEPS `relative_humidity_2m` takes the percent rule below): the
    // fraction-valued twin of the percent palette. The percent palette
    // over 0..1 would overlap its stops, so
    // #823 clips instead of rescaling and the field sits in its bottom 1 %
    // (#872). `contains` names relative humidity explicitly: "Specific
    // humidity" must not reach these ranges.
    DefaultRule {
        names: RH_NAMES,
        contains: RH_CONTAINS,
        palette: "humidity_fraction",
        unit_ranges: &[(FRACTION, 0.0, 1.0)],
        fallback_range: None,
    },
    // Relative humidity in percent. A bare `Humidity` short name with no
    // known unit still matches.
    DefaultRule {
        names: RH_NAMES,
        contains: RH_CONTAINS,
        palette: "humidity",
        unit_ranges: &[(PERCENT, 0.0, 100.0)],
        fallback_range: Some((0.0, 100.0)),
    },
    // Cloud and fog cover as a fraction, on the fraction-valued twin of the
    // percent palette (as for relative humidity above), including a CF area
    // fraction in unit 1 that a source serves unconverted.
    DefaultRule {
        names: CLOUD_NAMES,
        contains: &[
            "cloudcover",
            "cloudiness",
            "totalcloud",
            "cloudareafraction",
            "fogareafraction",
        ],
        palette: "cloud_cover_fraction",
        unit_ranges: &[(FRACTION, 0.0, 1.0)],
        fallback_range: None,
    },
    // The CF area fractions in percent, as engine-netcdf and engine-zarr
    // serve them from their standard name (MEPS `cloud_area_fraction`).
    // Unit-gated: CF's unit for them is 1, so without a known unit their
    // names guess nothing (unlike the cover names below, whose percent
    // fallback serves a source without units). A title naming cloud cover
    // still reaches that fallback.
    DefaultRule {
        names: &[],
        contains: CF_CLOUD_CONTAINS,
        palette: "cloud_cover",
        unit_ranges: &[(PERCENT, 0.0, 100.0)],
        fallback_range: None,
    },
    // Cloud cover in percent.
    DefaultRule {
        names: CLOUD_NAMES,
        contains: CLOUD_CONTAINS,
        palette: "cloud_cover",
        unit_ranges: &[(PERCENT, 0.0, 100.0)],
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
        let unit_n = unit.map(unit_key);
        // Config aliases also match as plain normalized strings, as they did
        // before unit keys: a key-like alias (`ms1`, `degc`) keeps working.
        let unit_plain = unit.map(normalize);

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
            let range = match resolve_range_owned(rule, unit_n.as_deref(), unit_plain.as_deref()) {
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

/// As [`resolve_range`], for a config rule: an alias matches the hint's
/// [`unit_key`], or its plain [`normalize`]d spelling (`plain`) when that is
/// not empty, so a symbol-only `%` alias never matches a `-` unit.
fn resolve_range_owned(
    rule: &DefaultOverride,
    unit: Option<&str>,
    plain: Option<&str>,
) -> Result<Option<(f64, f64)>, ()> {
    if rule.unit_ranges.is_empty() {
        return Ok(rule.fallback_range);
    }
    if let Some(u) = unit {
        let plain = plain.filter(|p| !p.is_empty());
        for (aliases, min, max) in &rule.unit_ranges {
            if aliases
                .iter()
                .any(|a| unit_key(a) == u || plain.is_some_and(|p| normalize(a) == p))
            {
                return Ok(Some((*min, *max)));
            }
        }
    }
    match rule.fallback_range {
        Some(r) => Ok(Some(r)),
        None => Err(()),
    }
}

/// The key a unit is matched by, in the embedded table and in config
/// unit ranges alike: its canonical spelling
/// (`ds_core::units::canonical_unit`), [`normalize`]d, with `%` read as
/// `percent`. `kg/m^2` → `kgm2`, `m/s` → `ms1`, `degC` → `c`,
/// `%` → `percent`; a unit with no letters or digits (`-`) → `""`.
pub fn unit_key(unit: &str) -> String {
    normalize(&ds_core::units::canonical_unit(unit).replace('%', "percent"))
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
        assert_eq!(k.range, Some((183.15, 323.15)));
        // …with °C → Celsius range (degree sign normalized away).
        let c = d.match_default("t2m", "", Some("°C")).unwrap();
        assert_eq!(c.range, Some((-90.0, 50.0)));
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

    /// #948: every name a derived geopotential height gets renders with the
    /// geopotential-height default over 0–21 000 gpm, as the native heights
    /// it never duplicates do; the geopotential it is derived from, in
    /// m² s⁻², over the same heights times g₀.
    #[test]
    fn derived_geopotential_heights_get_the_height_default() {
        let d = builtin();
        for (name, title) in [
            ("gh", "Geopotential height"),
            ("gh", "Geopotential height (surface)"),
            ("HGT", "Geopotential height (surface)"),
            ("geopotential_height", "Geopotential height"),
            ("geopotential_height_pl", "Geopotential height"),
        ] {
            let m = d
                .match_default(name, title, Some("gpm"))
                .unwrap_or_else(|| panic!("no default for {name}"));
            assert_eq!(m.palette, "viridis", "{name}");
            assert_eq!(m.range, Some((0.0, GEOPOTENTIAL_HEIGHT_MAX)), "{name}");
        }
        for (name, unit) in [
            ("z", "m2 s-2"),
            // wgrib2's abbreviation.
            ("GP", "m2 s-2"),
            ("geopotential_pl", "m^2/s^2"),
        ] {
            let m = d.match_default(name, "Geopotential", Some(unit)).unwrap();
            assert_eq!(m.range, Some((0.0, GEOPOTENTIAL_MAX)), "{name}");
        }
        assert!((GEOPOTENTIAL_MAX - 205_939.65).abs() < 1e-6);
    }

    /// #897: every name and title a derived wind speed gets, and the native
    /// speeds it never duplicates, render as wind speed over 0–40 m/s. A
    /// derived direction is no map layer and matches nothing.
    #[test]
    fn derived_wind_speeds_get_the_wind_speed_default() {
        let d = builtin();
        for (name, title, unit) in [
            ("10si", "Wind speed (10 m above ground)", Some("m s-1")),
            ("100si", "Wind speed (100 m above ground)", Some("m s-1")),
            ("ws", "Wind speed", Some("m s-1")),
            ("WIND", "Wind speed (10 m above ground)", Some("m s-1")),
            // A native GFS speed before its header has been probed.
            ("WIND", "WIND", None),
            ("si10", "Wind speed", Some("m s**-1")),
            // FMI's name, with its number's unit and with none.
            ("WindSpeedMS", "Wind speed", Some("m s-1")),
            ("WindSpeedMS", "Wind speed", None),
            ("wind_speed_10m", "Wind speed", Some("m s-1")),
            ("10m_wind_speed", "Wind speed", Some("m/s")),
        ] {
            let m = d
                .match_default(name, title, unit)
                .unwrap_or_else(|| panic!("no default for {name}"));
            assert_eq!(m.palette, "wind_speed", "{name}");
            assert_eq!(m.range, Some((0.0, 40.0)), "{name}");
        }
        for (name, title) in [
            ("10wdir", "Wind direction (10 m above ground)"),
            ("WDIR", "Wind direction"),
            ("WindDirection", "Wind direction"),
        ] {
            assert_eq!(d.match_default(name, title, Some("°")), None, "{name}");
        }
    }

    /// #1215: FMI newbase's u and v components, `WindUMS` / `WindVMS`, are
    /// signed winds on the u/v rule's diverging scale, and its
    /// `HourlyMaximumGust` a speed, both by unit like the other winds.
    /// QueryData lists each descriptor as its own title.
    #[test]
    fn newbase_wind_components_and_gusts_get_the_wind_defaults() {
        let d = builtin();
        for name in ["WindUMS", "WindVMS"] {
            for (unit, range) in [
                (Some("m s-1"), (-40.0, 40.0)),
                (Some("m/s"), (-40.0, 40.0)),
                (Some("kt"), (-80.0, 80.0)),
            ] {
                let m = d
                    .match_default(name, name, unit)
                    .unwrap_or_else(|| panic!("no default for {name} in {unit:?}"));
                assert_eq!(
                    (m.palette.as_str(), m.range),
                    ("diverging", Some(range)),
                    "{name}"
                );
            }
            // Signed: no unit, no guess.
            assert_eq!(d.match_default(name, name, None), None, "{name}");
        }
        for (unit, range) in [
            (Some("m s-1"), (0.0, 40.0)),
            (Some("kt"), (0.0, 80.0)),
            // The speed rule's own fallback.
            (None, (0.0, 40.0)),
        ] {
            let m = d
                .match_default("HourlyMaximumGust", "HourlyMaximumGust", unit)
                .unwrap_or_else(|| panic!("no default for HourlyMaximumGust in {unit:?}"));
            assert_eq!(
                (m.palette.as_str(), m.range),
                ("wind_speed", Some(range)),
                "{unit:?}"
            );
        }
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

    /// The events shape's strike-age layer (#504): age in minutes on the
    /// `lightning_age` palette over its one-hour stops; other units say
    /// nothing about age.
    #[test]
    fn lightning_age_in_minutes_takes_its_palette() {
        let d = builtin();
        let m = d.match_default("lightning_age", "", Some("min")).unwrap();
        assert_eq!(m.palette, "lightning_age");
        assert_eq!(m.range, Some((0.0, 60.0)));
        assert_eq!(d.match_default("lightning_age", "", None), None);
        assert_eq!(d.match_default("lightning_age", "", Some("s")), None);
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
                ("viridis", (0.0, GEOPOTENTIAL_MAX)),
            ),
            (
                "z",
                "Geopotential",
                "m2 s-2",
                ("viridis", (0.0, GEOPOTENTIAL_MAX)),
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

    /// The relative humidity and dew point `ds_core::moisture` derives
    /// (#898), by every name and title it gives them, in % and in the °C or
    /// K of their temperature, take the humidity and temperature defaults.
    #[test]
    fn derived_humidity_takes_the_humidity_and_temperature_defaults() {
        let d = builtin();
        let rh = "Relative humidity over water";
        for (name, title) in [
            ("2r", format!("{rh} (2 m above ground)")),
            ("r", rh.to_string()),
            ("r2", rh.to_string()),
            ("RH", format!("{rh} (2 m above ground)")),
            ("Humidity", rh.to_string()),
            ("relative_humidity_2m", format!("{rh} (2 m)")),
            ("relative_humidity_pl", rh.to_string()),
        ] {
            assert_eq!(
                d.match_default(name, &title, Some("%")),
                Some(ResolvedDefault {
                    palette: "humidity".into(),
                    range: Some((0.0, 100.0)),
                }),
                "{name}"
            );
        }
        for (name, title) in [
            ("2d", "Dew point temperature (2 m above ground)"),
            ("dpt", "Dew point temperature"),
            ("DPT", "Dew point temperature (2 m above ground)"),
            ("DewPoint", "Dew point temperature"),
            ("dew_point_temperature_2m", "Dew point temperature (2 m)"),
            ("dew_point_temperature_pl", "Dew point temperature"),
        ] {
            for (unit, range) in [("°C", (-90.0, 50.0)), ("K", (183.15, 323.15))] {
                assert_eq!(
                    d.match_default(name, title, Some(unit)),
                    Some(ResolvedDefault {
                        palette: "temperature".into(),
                        range: Some(range),
                    }),
                    "{name} in {unit}"
                );
            }
            // By name alone too, whatever the title.
            assert_eq!(
                d.match_default(name, "", Some("°C")).map(|r| r.palette),
                Some("temperature".into()),
                "{name}"
            );
        }
    }

    /// Relative humidity keeps its percent and fraction ranges, and a
    /// unit-less FMI `Humidity` its percent fallback; MSLP keeps
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
                "humidity_fraction",
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

    /// #1213: a precipitation rate stored in kg m-2 s-1, in any spelling,
    /// is served in mm/h (`ds_core::units::display_conversion`, engine-grib's
    /// twin), whose range the rate rule keys on; the stored unit has none.
    #[test]
    fn a_served_precipitation_rate_is_ranged_in_mm_per_hour() {
        let rule = RULES
            .iter()
            .find(|r| r.palette == "precipitation_rate")
            .unwrap();
        for spelling in ["kg m-2 s-1", "kg m**-2 s**-1", "kg/m^2/s", "kg/(m2 s)"] {
            let served = ds_core::units::display_conversion(spelling).unwrap().unit;
            assert_eq!(served, "mm/h", "{spelling}");
            let key = unit_key(served);
            assert!(
                rule.unit_ranges
                    .iter()
                    .any(|(aliases, min, max)| aliases.contains(&key.as_str())
                        && (*min, *max) == (0.0, 30.0)),
                "{key}"
            );
            let stored = unit_key(spelling);
            assert!(
                !rule
                    .unit_ranges
                    .iter()
                    .any(|(aliases, ..)| aliases.contains(&stored.as_str())),
                "{stored}"
            );
        }
    }

    /// #1067 PR 6: a unit matches in every spelling (`unit_key`), in the
    /// embedded table and in config unit ranges alike.
    #[test]
    fn unit_spellings_match_one_rule() {
        let d = builtin();
        for (name, title, spellings, palette, range) in [
            (
                "t2m",
                "",
                &["K", "kelvin", "degK"][..],
                "temperature",
                (183.15, 323.15),
            ),
            (
                "t2m",
                "",
                &["°C", "degC", "degree_Celsius", "Cel", "C"][..],
                "temperature",
                (-90.0, 50.0),
            ),
            (
                "ws",
                "",
                &["m s-1", "m/s", "m s**-1", "m s^-1", "m.s-1", "m·s⁻¹"][..],
                "wind_speed",
                (0.0, 40.0),
            ),
            (
                "u",
                "",
                &["m s-1", "m/s", "m s^-1"][..],
                "diverging",
                (-40.0, 40.0),
            ),
            (
                "prate",
                "",
                &["mm/h", "mm h-1", "mm/hr", "mm hr-1"][..],
                "precipitation_rate",
                (0.0, 30.0),
            ),
            (
                "w",
                "Vertical velocity",
                &["Pa s-1", "Pa/s", "Pa s**-1"][..],
                "diverging",
                (-2.0, 2.0),
            ),
            (
                "vo",
                "",
                &["s-1", "/s", "1/s", "s**-1"][..],
                "diverging",
                (-2e-4, 2e-4),
            ),
            (
                "z",
                "",
                &["m2 s-2", "m**2 s**-2", "m^2/s^2", "m²/s²"][..],
                "viridis",
                (0.0, GEOPOTENTIAL_MAX),
            ),
            (
                "tcwv",
                "",
                &["kg m-2", "kg/m^2", "kg m**-2", "kg.m-2", "mm"][..],
                "viridis",
                (0.0, 70.0),
            ),
            (
                "q",
                "",
                &["kg kg-1", "kg/kg", "1"][..],
                "viridis",
                (0.0, 0.02),
            ),
            ("rh", "", &["%", "percent"][..], "humidity", (0.0, 100.0)),
        ] {
            for unit in spellings {
                let m = d
                    .match_default(name, title, Some(unit))
                    .unwrap_or_else(|| panic!("no default for {name} in {unit:?}"));
                assert_eq!(m.palette, palette, "{name} in {unit:?}");
                assert_eq!(m.range, Some(range), "{name} in {unit:?}");
            }
        }
        // A config unit range in one spelling matches the others.
        let d = ParameterDefaults::with_overrides(vec![DefaultOverride {
            names: vec!["gust".into()],
            contains: vec![],
            palette: "viridis".into(),
            unit_ranges: vec![(vec!["m/s".into()], 0.0, 30.0)],
            fallback_range: None,
        }]);
        for unit in ["m/s", "m s-1", "m s**-1"] {
            assert_eq!(
                d.match_default("gust", "", Some(unit)).unwrap(),
                ResolvedDefault {
                    palette: "viridis".into(),
                    range: Some((0.0, 30.0)),
                },
                "{unit}"
            );
        }
        // …and never another unit: km/h falls through to the embedded rule,
        // which has no km/h range either and falls back to m/s.
        assert_eq!(
            d.match_default("gust", "", Some("km/h")).unwrap().palette,
            "wind_speed"
        );
    }

    /// A config alias written in the key-like form the embedded table uses
    /// (`ms1`, `degc`, `pas1`) still matches every unit it matched before
    /// unit keys: those whose plain normalized spelling it is. A symbol-only
    /// `%` alias matches `%` and `percent`, never a `-` unit.
    #[test]
    fn key_like_config_aliases_keep_matching() {
        for (alias, units, missed) in [
            (
                "ms1",
                &["m s-1", "m s**-1", "m s^-1", "m.s-1"][..],
                &["m s-2", "K"][..],
            ),
            ("degc", &["degC", "deg C", "deg_C"][..], &["K"][..]),
            ("pas1", &["Pa s-1", "Pa s**-1"][..], &["Pa"][..]),
            ("mmh1", &["mm h-1", "mm h**-1"][..], &["mm"][..]),
            ("%", &["%", "percent", " % "][..], &["-", "~", "1"][..]),
        ] {
            let d = ParameterDefaults::with_overrides(vec![DefaultOverride {
                names: vec!["param".into()],
                contains: vec![],
                palette: "viridis".into(),
                unit_ranges: vec![(vec![alias.into()], 1.0, 2.0)],
                fallback_range: None,
            }]);
            for unit in units {
                assert_eq!(
                    d.match_default("param", "", Some(unit)).map(|m| m.range),
                    Some(Some((1.0, 2.0))),
                    "{alias} vs {unit:?}"
                );
            }
            for unit in missed {
                assert_eq!(
                    d.match_default("param", "", Some(unit)),
                    None,
                    "{alias} vs {unit:?}"
                );
            }
        }
    }

    /// #1067 PR 6: MET Norway's MEPS sfc variables, with the titles and
    /// units engine-netcdf reports for them (`long_name` as in
    /// testdata/meps-netcdf, or the example's `[netcdf.titles]`; the files'
    /// `units` in their display unit, #1209: K as °C, Pa as hPa, kg/m^2 as
    /// mm, and unit 1 as % for relative humidity and the cloud and fog area
    /// fractions, whose standard names say they are fractions), and the
    /// files' own spellings, as a CF store that serves them unconverted
    /// would.
    #[test]
    fn meps_names_get_their_defaults() {
        let d = builtin();
        for (name, title, unit, palette, range) in [
            (
                "air_temperature_2m",
                "Screen level temperature (T2M)",
                "°C",
                "temperature",
                Some((-90.0, 50.0)),
            ),
            (
                "air_temperature_2m",
                "Screen level temperature (T2M)",
                "K",
                "temperature",
                Some((183.15, 323.15)),
            ),
            (
                "relative_humidity_2m",
                "Screen level relative humidity (RH2M)",
                "%",
                "humidity",
                Some((0.0, 100.0)),
            ),
            (
                "relative_humidity_2m",
                "Screen level relative humidity (RH2M)",
                "1",
                "humidity_fraction",
                Some((0.0, 1.0)),
            ),
            (
                "x_wind_10m",
                "10 metre wind along x-coordinate (U10M)",
                "m/s",
                "diverging",
                Some((-40.0, 40.0)),
            ),
            (
                "y_wind_10m",
                "10 m wind along the grid's y axis",
                "m/s",
                "diverging",
                Some((-40.0, 40.0)),
            ),
            (
                "x_wind",
                "x_wind",
                "m s-1",
                "diverging",
                Some((-40.0, 40.0)),
            ),
            (
                "wind_speed_of_gust",
                "Wind gust",
                "m/s",
                "wind_speed",
                Some((0.0, 40.0)),
            ),
            (
                "air_pressure_at_sea_level",
                "Mean Sea Level Pressure (MSLP)",
                "hPa",
                "pressure",
                Some((950.0, 1050.0)),
            ),
            (
                "air_pressure_at_sea_level",
                "Mean Sea Level Pressure (MSLP)",
                "Pa",
                "pressure",
                Some((95_000.0, 105_000.0)),
            ),
            (
                "precipitation_amount_acc",
                "Accumulated total precipitation",
                "mm",
                "precipitation_accumulation",
                Some((0.0, 200.0)),
            ),
            (
                "precipitation_amount_acc",
                "Accumulated total precipitation",
                "kg/m^2",
                "precipitation_accumulation",
                Some((0.0, 200.0)),
            ),
            (
                "precipitation_amount_acc",
                "Precipitation accumulated since run start",
                "kg/m^2",
                "precipitation_accumulation",
                Some((0.0, 200.0)),
            ),
            (
                "snowfall_amount_acc",
                "Accumulated snowfall",
                "kg/m^2",
                "precipitation_accumulation",
                Some((0.0, 200.0)),
            ),
            (
                "cloud_area_fraction",
                "Total cloud cover (TCC)",
                "%",
                "cloud_cover",
                Some((0.0, 100.0)),
            ),
            (
                "cloud_area_fraction",
                "Total cloud cover (TCC)",
                "1",
                "cloud_cover_fraction",
                Some((0.0, 1.0)),
            ),
            (
                "low_type_cloud_area_fraction",
                "Low cloud cover (LCC)",
                "%",
                "cloud_cover",
                Some((0.0, 100.0)),
            ),
            (
                "low_type_cloud_area_fraction",
                "Low cloud cover (LCC)",
                "1",
                "cloud_cover_fraction",
                Some((0.0, 1.0)),
            ),
            (
                "fog_area_fraction",
                "Fog",
                "%",
                "cloud_cover",
                Some((0.0, 100.0)),
            ),
            (
                "fog_area_fraction",
                "Fog",
                "1",
                "cloud_cover_fraction",
                Some((0.0, 1.0)),
            ),
        ] {
            let m = d
                .match_default(name, title, Some(unit))
                .unwrap_or_else(|| panic!("no default for {name}"));
            assert_eq!(m.palette, palette, "{name} / {title}");
            assert_eq!(m.range, range, "{name} / {title}");
        }
    }

    /// #1067 PR 8: MEPS's pressure-level variables, with the titles and units
    /// engine-netcdf reports for them (testdata/meps-netcdf-pl; temperature
    /// in °C and relative humidity in %, #1209, and in the files' K and 1;
    /// geopotential as stored, m^2/s^2, #948, and in gpm, and the height
    /// derived from it in gpm). Their wind
    /// components and the speed derived from them span ±80 / 0–80 m/s, twice
    /// the surface scale, for the jet; temperature, geopotential and
    /// relative humidity keep their all-levels ranges. The surface
    /// components and speeds keep theirs.
    #[test]
    fn meps_pressure_level_names_get_their_defaults() {
        let d = builtin();
        for (name, title, unit, palette, range) in [
            (
                "x_wind_pl",
                "Wind along x-coordinate on pressure levels",
                "m/s",
                "diverging",
                Some((-80.0, 80.0)),
            ),
            (
                "y_wind_pl",
                "Wind along y-coordinate on pressure levels",
                "m/s",
                "diverging",
                Some((-80.0, 80.0)),
            ),
            (
                "wind_speed_pl",
                "Wind speed",
                "m/s",
                "wind_speed",
                Some((0.0, 80.0)),
            ),
            (
                "air_temperature_pl",
                "Air temperature pressure levels",
                "°C",
                "temperature",
                Some((-90.0, 50.0)),
            ),
            (
                "air_temperature_pl",
                "Air temperature pressure levels",
                "K",
                "temperature",
                Some((183.15, 323.15)),
            ),
            (
                "geopotential_pl",
                "Geopotential pressure levels",
                "gpm",
                "viridis",
                Some((0.0, 21_000.0)),
            ),
            (
                "geopotential_pl",
                "Geopotential pressure levels",
                "m^2/s^2",
                "viridis",
                Some((0.0, GEOPOTENTIAL_MAX)),
            ),
            (
                "geopotential_height_pl",
                "Geopotential height",
                "gpm",
                "viridis",
                Some((0.0, 21_000.0)),
            ),
            (
                "relative_humidity_pl",
                "Relative humidity pressure levels",
                "%",
                "humidity",
                Some((0.0, 100.0)),
            ),
            (
                "relative_humidity_pl",
                "Relative humidity pressure levels",
                "1",
                "humidity_fraction",
                Some((0.0, 1.0)),
            ),
            (
                "x_wind_10m",
                "10 metre wind along x-coordinate (U10M)",
                "m/s",
                "diverging",
                Some((-40.0, 40.0)),
            ),
            (
                "wind_speed_10m",
                "Wind speed (10 m)",
                "m/s",
                "wind_speed",
                Some((0.0, 40.0)),
            ),
            (
                "u",
                "U component of wind",
                "m s**-1",
                "diverging",
                Some((-40.0, 40.0)),
            ),
        ] {
            let m = d
                .match_default(name, title, Some(unit))
                .unwrap_or_else(|| panic!("no default for {name}"));
            assert_eq!(m.palette, palette, "{name} / {title}");
            assert_eq!(m.range, range, "{name} / {title}");
        }
        // The components are unit-gated: a pressure title never reaches the
        // MSLP rule's pressure range.
        assert!(d
            .match_default(
                "x_wind_pl",
                "Wind along x-coordinate on pressure levels",
                None
            )
            .is_none());
    }

    /// The CF cloud and fog fractions: 0..1 in unit `1`, 0..100 in percent.
    /// CF's unit for them being 1, their names guess nothing without a known
    /// unit, nor in a unit with no letters (`-` is not `%`). A title that
    /// names cloud cover still takes the cover rule's percent fallback, as
    /// does any cloud-cover title without a unit.
    #[test]
    fn cloud_and_fog_fractions_follow_their_unit() {
        let d = builtin();
        for name in [
            "cloud_area_fraction",
            "high_type_cloud_area_fraction",
            "medium_type_cloud_area_fraction",
            "convective_cloud_area_fraction",
            "fog_area_fraction",
        ] {
            let fraction = d.match_default(name, "", Some("1")).unwrap();
            assert_eq!(
                (fraction.palette.as_str(), fraction.range),
                ("cloud_cover_fraction", Some((0.0, 1.0))),
                "{name}"
            );
            let percent = d.match_default(name, "", Some("%")).unwrap();
            assert_eq!(
                (percent.palette.as_str(), percent.range),
                ("cloud_cover", Some((0.0, 100.0))),
                "{name}"
            );
            assert_eq!(d.match_default(name, "", None), None, "{name}");
            for unit in ["m", "-", "~"] {
                assert_eq!(d.match_default(name, "", Some(unit)), None, "{name} {unit}");
            }
        }
        // MEPS's own title, in unit 1 and with no unit.
        let title = "Total cloud cover (TCC)";
        let fraction = d
            .match_default("cloud_area_fraction", title, Some("1"))
            .unwrap();
        assert_eq!(
            (fraction.palette.as_str(), fraction.range),
            ("cloud_cover_fraction", Some((0.0, 1.0)))
        );
        let untitled = d.match_default("cloud_area_fraction", title, None).unwrap();
        assert_eq!(
            (untitled.palette.as_str(), untitled.range),
            ("cloud_cover", Some((0.0, 100.0)))
        );
        for (name, unit, palette, range) in [
            ("tcc", None, "cloud_cover", (0.0, 100.0)),
            ("tcc", Some("%"), "cloud_cover", (0.0, 100.0)),
            ("tcc", Some("(0 - 1)"), "cloud_cover_fraction", (0.0, 1.0)),
            ("TotalCloudCover", None, "cloud_cover", (0.0, 100.0)),
        ] {
            let m = d.match_default(name, "", unit).unwrap();
            assert_eq!(
                (m.palette.as_str(), m.range),
                (palette, Some(range)),
                "{name}"
            );
        }
    }

    /// Run-start accumulations are unit-gated and exact: per-step
    /// precipitation keeps its 0..50 mm, and an `_acc` name in another unit
    /// gets the per-step rule as before rather than a guess.
    #[test]
    fn run_start_accumulations_have_their_own_range() {
        let d = builtin();
        for unit in ["kg m-2", "kg/m^2", "kg m**-2", "mm"] {
            let m = d
                .match_default("precipitation_amount_acc", "", Some(unit))
                .unwrap();
            assert_eq!(
                (m.palette.as_str(), m.range),
                ("precipitation_accumulation", Some((0.0, 200.0))),
                "{unit}"
            );
        }
        for (name, title, unit) in [
            ("tp", "Total precipitation", Some("mm")),
            (
                "precipitation_amount",
                "Precipitation amount",
                Some("kg m-2"),
            ),
            ("Precipitation1h", "Precipitation1h", None),
            ("precipitation_amount_acc", "", Some("m")),
            // A title saying "accumulated" says nothing about since when.
            (
                "apcp",
                "Precipitation amount accumulated over 3 h",
                Some("kg m-2"),
            ),
            // engine-netcdf's per-step amounts of the run-start
            // accumulations above (#1165), as it names, titles and serves
            // them: graupel by its title alone.
            (
                "precipitation_amount",
                "Precipitation amount (1 h accumulation)",
                Some("mm"),
            ),
            (
                "rainfall_amount",
                "Rainfall amount (1 h accumulation)",
                Some("mm"),
            ),
            (
                "snowfall_amount",
                "Snowfall amount (1 h to 3 h accumulation per forecast step)",
                Some("mm"),
            ),
            (
                "graupelfall_amount",
                "Graupelfall amount (accumulation per forecast step)",
                Some("mm"),
            ),
        ] {
            let m = d.match_default(name, title, unit).unwrap();
            assert_eq!(
                (m.palette.as_str(), m.range),
                ("precipitation", Some((0.0, 50.0))),
                "{name}"
            );
        }
        // Per-step snowfall stays on the per-step scale and stays unit-gated.
        assert_eq!(
            d.match_default("sf", "Snowfall", Some("kg/m^2"))
                .unwrap()
                .range,
            Some((0.0, 50.0))
        );
        assert_eq!(d.match_default("snowfall_amount_acc", "", Some("m")), None);
    }

    /// GRIB (display units), Zarr (CF units as stored) and QueryData (its
    /// FMI parameter numbers' units, #1211) parameters keep the defaults
    /// they had before units were normalised, apart from fractions, which
    /// leave the percent palettes' bottom 1 % for their fraction twins
    /// (#872). QueryData's unit-gated temperature and pressure match now
    /// that it reports units.
    #[test]
    fn engine_parameters_keep_their_defaults() {
        let d = builtin();
        for (name, title, unit, palette, range) in [
            // GRIB, after engine-grib's display conversion.
            (
                "2t",
                "Temperature (2 m above ground)",
                Some("°C"),
                "temperature",
                Some((-90.0, 50.0)),
            ),
            (
                "TMP",
                "Temperature",
                Some("°C"),
                "temperature",
                Some((-90.0, 50.0)),
            ),
            (
                "msl",
                "Pressure reduced to MSL",
                Some("hPa"),
                "pressure",
                Some((950.0, 1050.0)),
            ),
            (
                "10u",
                "u-component of wind",
                Some("m s-1"),
                "diverging",
                Some((-40.0, 40.0)),
            ),
            (
                "10si",
                "Wind speed",
                Some("m s-1"),
                "wind_speed",
                Some((0.0, 40.0)),
            ),
            (
                "tp",
                "Total precipitation",
                Some("mm"),
                "precipitation",
                Some((0.0, 50.0)),
            ),
            (
                "PRATE",
                "Precipitation rate",
                Some("mm/h"),
                "precipitation_rate",
                Some((0.0, 30.0)),
            ),
            (
                "r",
                "Relative humidity",
                Some("%"),
                "humidity",
                Some((0.0, 100.0)),
            ),
            (
                "tcc",
                "Total cloud cover",
                Some("%"),
                "cloud_cover",
                Some((0.0, 100.0)),
            ),
            (
                "q",
                "Specific humidity",
                Some("kg kg-1"),
                "viridis",
                Some((0.0, 0.02)),
            ),
            (
                "gh",
                "Geopotential height",
                Some("gpm"),
                "viridis",
                Some((0.0, 21_000.0)),
            ),
            (
                "cape",
                "Convective available potential energy",
                Some("J kg-1"),
                "viridis",
                Some((0.0, 4000.0)),
            ),
            // Zarr: CF units as the store spells them.
            (
                "t2m",
                "2 metre temperature",
                Some("K"),
                "temperature",
                Some((183.15, 323.15)),
            ),
            (
                "2m_temperature",
                "2 metre temperature",
                Some("K"),
                "temperature",
                Some((183.15, 323.15)),
            ),
            (
                "u10",
                "10 metre U wind component",
                Some("m s**-1"),
                "diverging",
                Some((-40.0, 40.0)),
            ),
            (
                "mean_sea_level_pressure",
                "Mean sea level pressure",
                Some("Pa"),
                "pressure",
                Some((95_000.0, 105_000.0)),
            ),
            (
                "si10",
                "10 metre wind speed",
                Some("m s**-1"),
                "wind_speed",
                Some((0.0, 40.0)),
            ),
            (
                "tcwv",
                "Total column vertically-integrated water vapour",
                Some("kg m**-2"),
                "viridis",
                Some((0.0, 70.0)),
            ),
            (
                "z",
                "Geopotential",
                Some("m**2 s**-2"),
                "viridis",
                Some((0.0, GEOPOTENTIAL_MAX)),
            ),
            // QueryData: FMI names, their numbers' units.
            (
                "Temperature",
                "Temperature",
                Some("°C"),
                "temperature",
                Some((-90.0, 50.0)),
            ),
            (
                "DewPoint",
                "DewPoint",
                Some("°C"),
                "temperature",
                Some((-90.0, 50.0)),
            ),
            (
                "Pressure",
                "Pressure",
                Some("hPa"),
                "pressure",
                Some((950.0, 1050.0)),
            ),
            (
                "Humidity",
                "Humidity",
                Some("%"),
                "humidity",
                Some((0.0, 100.0)),
            ),
            (
                "TotalCloudCover",
                "TotalCloudCover",
                Some("%"),
                "cloud_cover",
                Some((0.0, 100.0)),
            ),
            (
                "WindSpeedMS",
                "WindSpeedMS",
                Some("m s-1"),
                "wind_speed",
                Some((0.0, 40.0)),
            ),
            (
                "WindGust",
                "WindGust",
                Some("m s-1"),
                "wind_speed",
                Some((0.0, 40.0)),
            ),
            (
                "Precipitation1h",
                "Precipitation1h",
                Some("mm"),
                "precipitation",
                Some((0.0, 50.0)),
            ),
            // Radar.
            ("DBZH", "", Some("dBZ"), "radar_dbz", None),
            (
                "VRADH",
                "",
                Some("m/s"),
                "radial_velocity",
                Some((-48.0, 48.0)),
            ),
        ] {
            let m = d
                .match_default(name, title, unit)
                .unwrap_or_else(|| panic!("no default for {name}"));
            assert_eq!(m.palette, palette, "{name}");
            assert_eq!(m.range, range, "{name}");
        }
        // Unit-gated rules still never guess.
        for (name, title) in [
            ("Temperature", "Temperature"),
            ("Pressure", "Pressure"),
            ("WindUMS", "u-component of wind"),
        ] {
            assert_eq!(d.match_default(name, title, None), None, "{name}");
        }
    }

    #[test]
    fn unit_keys() {
        assert_eq!(unit_key("kg/m^2"), "kgm2");
        assert_eq!(unit_key("kg m**-2"), "kgm2");
        assert_eq!(unit_key("m/s"), "ms1");
        assert_eq!(unit_key("degC"), "c");
        assert_eq!(unit_key("percent"), "percent");
        assert_eq!(unit_key("%"), "percent");
        assert_eq!(unit_key("-"), "");
        assert_eq!(unit_key("(0 - 1)"), "01");
        assert_eq!(unit_key("1"), "1");
    }

    #[test]
    fn normalize_examples() {
        assert_eq!(normalize("10 m wind speed"), "10mwindspeed");
        assert_eq!(normalize("°C"), "c");
        assert_eq!(normalize("m s**-1"), "ms1");
        assert_eq!(normalize("kg m-2 s-1"), "kgm2s1");
    }
}
