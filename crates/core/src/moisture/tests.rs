use super::*;

mod derived;

const K: f64 = CELSIUS_ZERO;

/// Reference values from the Goff–Gratch equation for saturation over
/// liquid water, as WMO-No. 8 (1988) and the Smithsonian Meteorological
/// Tables give it: `log10 e = −7.90298 (Ts/T − 1) + 5.02808 log10(Ts/T)
/// − 1.3816e−7 (10^(11.344 (1 − T/Ts)) − 1) + 8.1328e−3
/// (10^(−3.49149 (Ts/T − 1)) − 1) + log10 1013.246`, Ts = 373.16 K, e in
/// hPa. It is a formula independent of the Tetens fit under test, computed
/// with Python's `math` (dew points by bisection on it). The IFS Tetens
/// constants stay within 0.3 % of it from −30 to +40 °C (0.8 % at −40 °C),
/// which bounds the tolerances below; an ice reference misses by 25 % at
/// −30 °C.
const GOFF_GRATCH_PA: [(f64, f64); 9] = [
    (-40.0, 18.894_4),
    (-30.0, 50.831_6),
    (-20.0, 125.292_5),
    (-10.0, 286.043_6),
    (0.0, 610.336_1),
    (10.0, 1_226.406_2),
    (20.0, 2_335.846_8),
    (30.0, 4_240.598_5),
    (40.0, 7_373.809_6),
];

fn assert_close(got: Option<f64>, want: f64, tolerance: f64, what: &str) {
    let got = got.unwrap_or_else(|| panic!("{what}: no value"));
    assert!(
        (got - want).abs() <= tolerance,
        "{what}: {got} is not within {tolerance} of {want}"
    );
}

#[test]
fn saturation_over_water_matches_goff_gratch() {
    for (celsius, want) in GOFF_GRATCH_PA {
        let got = saturation_vapour_pressure(celsius + K).unwrap();
        let tolerance = if celsius < -35.0 { 0.01 } else { 0.003 };
        assert!(
            (got / want - 1.0).abs() < tolerance,
            "{celsius} °C: {got} Pa vs {want} Pa"
        );
    }
    // Over ice it would be 37.93 Pa at −30 °C: a quarter lower.
    let ice = 611.21 * (22.587 * (-30.0 + K - 273.16) / (-30.0 + K + 0.7)).exp();
    assert!((ice / saturation_vapour_pressure(-30.0 + K).unwrap()) < 0.76);
    for t in [f64::NAN, f64::INFINITY, 0.0, TETENS_A4, -5.0] {
        assert_eq!(saturation_vapour_pressure(t), None, "{t}");
    }
}

/// Relative humidity from temperature and dew point, against Goff–Gratch
/// `100 · e_w(Td) / e_w(T)`, above and below 0 °C.
#[test]
fn relative_humidity_from_dew_point_matches_goff_gratch() {
    for (t, td, want) in [
        (20.0, 10.0, 52.503_7),
        (-10.0, -15.0, 66.779_9),
        (-30.0, -32.0, 82.651_2),
    ] {
        assert_close(
            relative_humidity_from_dew_point(t + K, td + K),
            want,
            0.15,
            &format!("T {t} °C, Td {td} °C"),
        );
    }
}

/// Dew point from temperature and relative humidity, against the
/// temperature at which Goff–Gratch saturates at `RH · e_w(T)`.
#[test]
fn dew_point_from_relative_humidity_matches_goff_gratch() {
    for (t, rh, want) in [(25.0, 60.0, 16.701_1), (-20.0, 80.0, -22.558_8)] {
        assert_close(
            dew_point_from_relative_humidity(t + K, rh).map(|td| td - K),
            want,
            0.1,
            &format!("T {t} °C, RH {rh} %"),
        );
    }
}

/// From specific humidity at a pressure: the vapour pressure
/// `q·p / (0.62198 + 0.37802·q)`, then Goff–Gratch over water. The second
/// case is below 0 °C, where an ice reference would read 49 %.
#[test]
fn specific_humidity_paths_match_goff_gratch() {
    for (t, p, q, rh, td) in [
        (5.0, 85_000.0, 0.005, 78.184_4, 1.521_7),
        (-25.0, 50_000.0, 0.000_4, 39.873_5, -34.747_6),
    ] {
        let what = format!("T {t} °C, p {p} Pa, q {q}");
        assert_close(
            relative_humidity_from_specific_humidity(t + K, q, p),
            rh,
            0.15,
            &what,
        );
        assert_close(
            dew_point_from_specific_humidity(t + K, q, p).map(|td| td - K),
            td,
            0.1,
            &what,
        );
    }
    assert!((vapour_pressure(0.005, 85_000.0) - 681.231_6).abs() < 1e-3);
}

/// One formula both ways: a dew point derived from a derived relative
/// humidity is the dew point it came from.
#[test]
fn dew_point_inverts_relative_humidity_exactly() {
    for (t, td) in [(30.0, 25.0), (20.0, -5.0), (-5.0, -12.0), (-40.0, -45.0)] {
        let rh = relative_humidity_from_dew_point(t + K, td + K).unwrap();
        let back = dew_point_from_relative_humidity(t + K, rh).unwrap();
        assert!((back - (td + K)).abs() < 1e-9, "{t} {td}: {back}");
    }
}

/// Maintainer decision (#898): relative humidity in 0–100 %, dew point at
/// most the temperature; nothing to take a logarithm of gives 0 % and no
/// dew point.
#[test]
fn bounds_are_clamped() {
    // A dew point interpolated a little above its temperature.
    assert_eq!(
        relative_humidity_from_dew_point(10.0 + K, 10.3 + K),
        Some(100.0)
    );
    // Supersaturated relative humidity, and specific humidity above
    // saturation, give the temperature.
    assert_eq!(
        dew_point_from_relative_humidity(5.0 + K, 104.0),
        Some(5.0 + K)
    );
    assert_eq!(
        relative_humidity_from_specific_humidity(0.0 + K, 0.02, 100_000.0),
        Some(100.0)
    );
    assert_eq!(
        dew_point_from_specific_humidity(0.0 + K, 0.02, 100_000.0),
        Some(K)
    );
    // NWP's negative specific humidity, and none at all.
    for q in [0.0, -1e-5] {
        assert_eq!(
            relative_humidity_from_specific_humidity(K, q, 85_000.0),
            Some(0.0)
        );
        assert_eq!(dew_point_from_specific_humidity(K, q, 85_000.0), None);
    }
    for rh in [0.0, -0.5] {
        assert_eq!(dew_point_from_relative_humidity(K, rh), None);
    }
    for (t, x, p) in [
        (f64::NAN, 0.005, 85_000.0),
        (K, f64::NAN, 85_000.0),
        (K, 0.005, 0.0),
        (K, 0.005, f64::NAN),
    ] {
        assert_eq!(relative_humidity_from_specific_humidity(t, x, p), None);
        assert_eq!(dew_point_from_specific_humidity(t, x, p), None);
    }
    assert_eq!(relative_humidity_from_dew_point(K, f64::NAN), None);
    assert_eq!(dew_point_from_relative_humidity(f64::NAN, 50.0), None);
}

/// Units come from the served unit: K or °C, % or a fraction, kg/kg or
/// g/kg, any spelling; the result is the same as in SI.
#[test]
fn recipes_convert_each_input_from_its_served_unit() {
    use RelativeHumidityUnit::{Fraction, Percent};
    use SpecificHumidityUnit::{GramPerKg, KgPerKg};
    use TemperatureUnit::{Celsius, Kelvin};
    let rh = |t, td, samples: [f64; 2]| {
        Recipe::RelativeHumidityFromDewPoint { t, td }.value(&samples.map(Some))
    };
    let want = relative_humidity_from_dew_point(293.15, 283.15);
    assert_eq!(rh(Kelvin, Kelvin, [293.15, 283.15]), want);
    let mixed = rh(Kelvin, Celsius, [293.15, 10.0]).unwrap();
    assert!((mixed - want.unwrap()).abs() < 1e-9);
    let td = |t, rh, samples: [f64; 2]| {
        Recipe::DewPointFromRelativeHumidity { t, rh }.value(&samples.map(Some))
    };
    let in_c = td(Celsius, Percent, [25.0, 60.0]).unwrap();
    assert!((in_c - (dew_point_from_relative_humidity(298.15, 60.0).unwrap() - K)).abs() < 1e-9);
    assert!((td(Celsius, Fraction, [25.0, 0.6]).unwrap() - in_c).abs() < 1e-9);
    let in_k = td(Kelvin, Percent, [298.15, 60.0]).unwrap();
    assert!(
        (in_k - K - in_c).abs() < 1e-9,
        "served in the temperature's unit"
    );
    let q = |q, samples: [f64; 3]| {
        Recipe::RelativeHumidityFromSpecificHumidity { t: Celsius, q }.value(&samples.map(Some))
    };
    let si = q(KgPerKg, [5.0, 0.005, 85_000.0]).unwrap();
    assert!((q(GramPerKg, [5.0, 5.0, 85_000.0]).unwrap() - si).abs() < 1e-9);
    let dpt = Recipe::DewPointFromSpecificHumidity {
        t: Celsius,
        q: GramPerKg,
    };
    assert!(dpt.reads_level_pressure());
    assert_eq!(dpt.quantity(), Quantity::DewPoint);
    assert!(dpt.value(&[Some(5.0), Some(5.0)]).is_none(), "no pressure");
    assert!(dpt.value(&[Some(5.0), None, Some(85_000.0)]).is_none());
    // Spellings, through `canonical_unit`.
    for (unit, want) in [
        ("K", Some(Kelvin)),
        ("degC", Some(Celsius)),
        ("°C", Some(Celsius)),
    ] {
        assert_eq!(TemperatureUnit::of(unit), want, "{unit}");
    }
    for unit in ["", "degF", "C", "1"] {
        assert_eq!(TemperatureUnit::of(unit), None, "{unit}");
    }
    assert_eq!(RelativeHumidityUnit::of("percent"), Some(Percent));
    assert_eq!(RelativeHumidityUnit::of("1"), Some(Fraction));
    assert_eq!(RelativeHumidityUnit::of("kg kg-1"), None);
    for (unit, want) in [
        ("kg kg-1", Some(KgPerKg)),
        ("kg/kg", Some(KgPerKg)),
        ("1", Some(KgPerKg)),
        ("g/kg", Some(GramPerKg)),
        ("g kg**-1", Some(GramPerKg)),
        ("%", None),
        ("g", None),
    ] {
        assert_eq!(SpecificHumidityUnit::of(unit), want, "{unit}");
    }
}

// --- Pairing and the plan ---------------------------------------------

fn facts(parameters: Vec<ParameterFacts>) -> WindFacts {
    WindFacts {
        grid: crate::wind::GridAxes::NorthAligned,
        parameters,
    }
}

/// A GRIB parameter at single level `level` (`None`: on the view's
/// pressure axis).
fn grib(name: &str, triple: (u8, u8, u8), unit: &str, level: Option<&str>) -> ParameterFacts {
    ParameterFacts {
        grib: Some(triple),
        unit: unit.into(),
        level: level.map(String::from),
        level_label: level.map(|_| "2 m above ground".into()),
        vertical: level.is_none().then_some(VerticalKind::Pressure),
        ..ParameterFacts::new(name)
    }
}

fn cf(name: &str, standard_name: &str, unit: &str, level: Option<&str>) -> ParameterFacts {
    ParameterFacts {
        standard_name: Some(standard_name.into()),
        unit: unit.into(),
        level: level.map(String::from),
        level_label: level.map(String::from),
        vertical: level.is_none().then_some(VerticalKind::Pressure),
        ..ParameterFacts::new(name)
    }
}

fn qd(name: &str, number: u32, unit: &str) -> ParameterFacts {
    ParameterFacts {
        fmi_param: Some(number),
        unit: unit.into(),
        ..ParameterFacts::new(name)
    }
}

const T: (u8, u8, u8) = (0, 0, 0);
const TD: (u8, u8, u8) = (0, 0, 6);
const Q: (u8, u8, u8) = (0, 1, 0);
const RH: (u8, u8, u8) = (0, 1, 1);
const HAG2: Option<&str> = Some("hag:2");

fn build(parameters: Vec<ParameterFacts>) -> MoisturePlan {
    MoisturePlan::build(&facts(parameters), &HashSet::new())
}

fn outcomes(plan: &MoisturePlan) -> Vec<String> {
    plan.outcomes.iter().map(ToString::to_string).collect()
}

fn derived(plan: &MoisturePlan) -> Vec<(&str, Vec<&str>)> {
    plan.derived
        .iter()
        .map(|d| {
            (
                d.name.as_str(),
                d.inputs.iter().map(String::as_str).collect(),
            )
        })
        .collect()
}

/// ECMWF single levels (IFS and AIFS open data): `2t` and `2d` give `2r`,
/// in %, titled with its saturation reference; `2d` is not derived again.
#[test]
fn ecmwf_2t_2d_give_2r() {
    let plan = build(vec![
        grib("2t", T, "°C", HAG2),
        grib("2d", TD, "°C", HAG2),
        grib("10u", (0, 2, 2), "m s-1", Some("hag:10")),
    ]);
    assert_eq!(derived(&plan), [("2r", vec!["2t", "2d"])]);
    let rh = &plan.derived[0];
    assert_eq!(
        rh.recipe,
        Recipe::RelativeHumidityFromDewPoint {
            t: TemperatureUnit::Celsius,
            td: TemperatureUnit::Celsius
        }
    );
    assert_eq!(rh.unit, "%");
    assert_eq!(rh.title, "Relative humidity over water (2 m above ground)");
    assert_eq!(
        outcomes(&plan),
        [
            "2t: derived relative humidity '2r' from '2t' and '2d'; no dew point: the \
          collection already has '2d'"
        ]
    );
}

/// ECMWF pressure levels: AIFS's `t` and `q` give `r` and `dpt` from the
/// level's pressure; IFS also has `r`, so only `dpt`, from `q`, which needs
/// no assumption about the reference of IFS's mixed-phase `r`.
#[test]
fn ecmwf_pressure_levels_derive_from_q_and_the_level() {
    let plan = build(vec![
        grib("t", T, "°C", None),
        grib("q", Q, "kg kg-1", None),
    ]);
    assert_eq!(
        derived(&plan),
        [("r", vec!["t", "q"]), ("dpt", vec!["t", "q"])]
    );
    assert!(plan.derived.iter().all(|d| d.recipe.reads_level_pressure()));
    assert_eq!(plan.derived[0].title, "Relative humidity over water");
    assert_eq!(
        (
            plan.derived[1].title.as_str(),
            plan.derived[1].unit.as_str()
        ),
        ("Dew point temperature", "°C")
    );
    assert_eq!(
        outcomes(&plan),
        [
            "t: derived relative humidity 'r' from 't', 'q' and the level's pressure; \
          derived dew point 'dpt' from 't', 'q' and the level's pressure"
        ]
    );
    let plan = build(vec![
        grib("t", T, "°C", None),
        grib("q", Q, "kg kg-1", None),
        grib("r", RH, "%", None),
    ]);
    assert_eq!(derived(&plan), [("dpt", vec!["t", "q"])]);
    // Without q, the dew point comes from r (MEPS-like).
    let plan = build(vec![grib("t", T, "°C", None), grib("r", RH, "%", None)]);
    assert_eq!(derived(&plan), [("dpt", vec!["t", "r"])]);
    assert_eq!(
        plan.derived[0].recipe,
        Recipe::DewPointFromRelativeHumidity {
            t: TemperatureUnit::Celsius,
            rh: RelativeHumidityUnit::Percent
        }
    );
}

/// wgrib2's abbreviations: GFS ships `TMP`, `DPT` and `RH` at 2 m, so
/// nothing is derived there; with one missing, it is.
#[test]
fn wgrib2_names_pair_at_any_level() {
    let plan = build(vec![
        grib("TMP", T, "°C", HAG2),
        grib("DPT", TD, "°C", HAG2),
        grib("RH", RH, "%", HAG2),
    ]);
    assert!(plan.derived.is_empty());
    assert_eq!(
        outcomes(&plan),
        [
            "TMP: no relative humidity: the collection already has 'RH'; no dew point: the \
          collection already has 'DPT'"
        ]
    );
    let plan = build(vec![
        grib("TMP", T, "°C", HAG2),
        grib("DPT", TD, "°C", HAG2),
    ]);
    assert_eq!(derived(&plan), [("RH", vec!["TMP", "DPT"])]);
    let plan = build(vec![grib("TMP", T, "K", HAG2), grib("RH", RH, "%", HAG2)]);
    assert_eq!(derived(&plan), [("DPT", vec!["TMP", "RH"])]);
    assert_eq!(plan.derived[0].unit, "K", "the temperature's unit");
    let plan = build(vec![
        grib("TMP", T, "°C", None),
        grib("SPFH", Q, "kg kg-1", None),
    ]);
    assert_eq!(
        derived(&plan),
        [("RH", vec!["TMP", "SPFH"]), ("DPT", vec!["TMP", "SPFH"])]
    );
}

/// CF variables are named in the store's vocabulary: the `air_temperature`
/// phrase swapped, as MEPS names its 2 m and pressure-level fields; ERA5's
/// netCDF names are ECMWF's.
#[test]
fn cf_names_follow_the_store_vocabulary() {
    let plan = build(vec![
        cf("air_temperature_2m", AIR_TEMPERATURE, "°C", Some("2 m")),
        cf("relative_humidity_2m", RELATIVE_HUMIDITY, "%", Some("2 m")),
    ]);
    assert_eq!(
        derived(&plan),
        [(
            "dew_point_temperature_2m",
            vec!["air_temperature_2m", "relative_humidity_2m"]
        )]
    );
    assert_eq!(plan.derived[0].title, "Dew point temperature (2 m)");
    let plan = build(vec![
        cf("air_temperature_pl", AIR_TEMPERATURE, "°C", None),
        cf("relative_humidity_pl", RELATIVE_HUMIDITY, "%", None),
    ]);
    assert_eq!(derived(&plan)[0].0, "dew_point_temperature_pl");
    let plan = build(vec![
        cf("air_temperature_pl", AIR_TEMPERATURE, "K", None),
        cf("specific_humidity_pl", SPECIFIC_HUMIDITY, "1", None),
    ]);
    assert_eq!(
        derived(&plan),
        [
            (
                "relative_humidity_pl",
                vec!["air_temperature_pl", "specific_humidity_pl"]
            ),
            (
                "dew_point_temperature_pl",
                vec!["air_temperature_pl", "specific_humidity_pl"]
            ),
        ]
    );
    let plan = build(vec![
        cf("t2m", AIR_TEMPERATURE, "°C", Some("2 m")),
        cf("d2m", DEW_POINT_TEMPERATURE, "°C", Some("2 m")),
    ]);
    assert_eq!(derived(&plan), [("r2", vec!["t2m", "d2m"])]);
    // A temperature whose name has no phrase and is in no vocabulary has no
    // partner to look for.
    let plan = build(vec![
        cf("tas", AIR_TEMPERATURE, "°C", Some("2 m")),
        cf("hurs", RELATIVE_HUMIDITY, "%", Some("2 m")),
    ]);
    assert!(plan.outcomes.is_empty());
}

/// QueryData: FMI numbers identify the ingredients and newbase names the
/// result; a temperature in another vocabulary keeps its own names.
#[test]
fn querydata_pairs_by_fmi_number() {
    let plan = build(vec![qd("Temperature", 4, "°C"), qd("DewPoint", 10, "°C")]);
    assert_eq!(
        derived(&plan),
        [("Humidity", vec!["Temperature", "DewPoint"])]
    );
    let plan = build(vec![qd("Temperature", 4, "°C"), qd("Humidity", 13, "%")]);
    assert_eq!(
        derived(&plan),
        [("DewPoint", vec!["Temperature", "Humidity"])]
    );
    // An ECMWF-converted file: `2t` by short name, its dew point found by
    // number whatever it is called.
    let plan = build(vec![
        qd("2t", 4, "°C"),
        ParameterFacts {
            edr_name: Some("Dew point".into()),
            ..qd("Dew point", 10, "°C")
        },
    ]);
    assert_eq!(derived(&plan), [("2r", vec!["2t", "Dew point"])]);
    assert_eq!(plan.derived[0].edr_inputs, ["2t", "Dew point"]);
    // A number with no unit refuses.
    let plan = build(vec![qd("Temperature", 4, ""), qd("Humidity", 13, "%")]);
    assert_eq!(
        outcomes(&plan),
        [
            "Temperature: no relative humidity: the collection already has 'Humidity'; no dew \
          point: the unit of 'Temperature' is not stated, or not read yet"
        ]
    );
}

/// "Missing" is by GRIB triple, CF standard name, FMI number, or a
/// vocabulary name on a parameter without metadata, at the same level.
#[test]
fn a_quantity_present_at_the_level_is_not_derived() {
    for existing in [
        grib("relhum", RH, "%", None),
        cf("hur", RELATIVE_HUMIDITY, "%", None),
        ParameterFacts {
            vertical: Some(VerticalKind::Pressure),
            ..ParameterFacts::new("r")
        },
    ] {
        let name = existing.name.clone();
        let plan = build(vec![
            grib("t", T, "°C", None),
            grib("q", Q, "kg kg-1", None),
            existing,
        ]);
        assert_eq!(derived(&plan), [("dpt", vec!["t", "q"])], "{name}");
        assert!(
            outcomes(&plan)[0].contains(&format!("the collection already has '{name}'")),
            "{name}"
        );
    }
    // At another level it is no duplicate.
    let plan = build(vec![
        grib("2t", T, "°C", HAG2),
        grib("2d", TD, "°C", HAG2),
        grib("RH", RH, "%", Some("pl:850")),
    ]);
    assert_eq!(derived(&plan), [("2r", vec!["2t", "2d"])]);
}

/// Same level only, a real pressure only, and identified ingredients only.
#[test]
fn ingredients_must_share_the_level_and_have_a_pressure() {
    let plan = build(vec![
        grib("2t", T, "°C", HAG2),
        grib("2d", TD, "°C", Some("sfc")),
    ]);
    assert!(plan.derived.is_empty());
    assert_eq!(
        outcomes(&plan),
        [
            "2t: no relative humidity: '2t' and '2d' are at different levels; no dew point: \
          no relative humidity or specific humidity at its level"
        ]
    );
    // q at a single level would need the surface pressure; mean sea-level
    // pressure never stands in for it.
    let plan = build(vec![
        grib("TMP", T, "°C", HAG2),
        grib("SPFH", Q, "kg kg-1", HAG2),
        grib("PRMSL", (0, 3, 1), "hPa", Some("msl")),
    ]);
    assert!(plan.derived.is_empty());
    assert_eq!(
        outcomes(&plan),
        [
            "TMP: nothing derived: 'SPFH' is not on pressure levels, and a single level's \
          pressure would be the surface pressure, which is not supported"
        ]
    );
    // Model levels.
    let model = |p: ParameterFacts| ParameterFacts {
        vertical: Some(VerticalKind::ModelLevel),
        ..p
    };
    let plan = build(vec![
        model(grib("t", T, "K", None)),
        model(grib("q", Q, "kg kg-1", None)),
    ]);
    assert!(plan.derived.is_empty());
    assert!(outcomes(&plan)[0].contains("on model levels"));
    // A vocabulary name whose metadata says otherwise.
    let plan = build(vec![grib("2t", T, "°C", HAG2), grib("2d", T, "°C", HAG2)]);
    assert_eq!(
        outcomes(&plan),
        [
            "2t: no relative humidity: the source's metadata does not identify '2d' as dew \
          point; no dew point: no relative humidity or specific humidity at its level"
        ]
    );
    // A temperature without ingredients is no candidate.
    assert!(build(vec![grib("2t", T, "°C", HAG2)]).outcomes.is_empty());
}

/// An ingredient whose unit is unknown refuses the derivation, with the
/// reason in the load log.
#[test]
fn unknown_units_refuse() {
    let plan = build(vec![grib("2t", T, "", HAG2), grib("2d", TD, "°C", HAG2)]);
    assert!(plan.derived.is_empty());
    assert_eq!(
        outcomes(&plan),
        [
            "2t: no relative humidity: the unit of '2t' is not stated, or not read yet; no dew \
          point: the collection already has '2d'"
        ]
    );
    let plan = build(vec![
        grib("2t", T, "°C", HAG2),
        grib("2d", TD, "degF", HAG2),
    ]);
    assert!(outcomes(&plan)[0].contains("the unit of '2d' is 'degF', not K or °C"));
    let plan = build(vec![grib("t", T, "°C", None), grib("q", Q, "g", None)]);
    assert!(plan.derived.is_empty());
    assert!(outcomes(&plan)[0].contains("the unit of 'q' is 'g', not kg kg-1, g kg-1 or 1"));
    let plan = build(vec![grib("t", T, "°C", None), grib("r", RH, "", None)]);
    assert!(outcomes(&plan)[0].contains("the unit of 'r' is not stated"));
}

/// A name another derivation or parameter holds is not taken again.
#[test]
fn names_already_taken_are_refused() {
    let claimed = HashSet::from(["2r".to_string()]);
    let plan = MoisturePlan::build(
        &facts(vec![grib("2t", T, "°C", HAG2), grib("2d", TD, "°C", HAG2)]),
        &claimed,
    );
    assert!(plan.derived.is_empty());
    assert!(outcomes(&plan)[0].contains("the name '2r' is taken by another parameter"));
    // A parameter named like the result but of another quantity.
    let plan = build(vec![
        grib("2t", T, "°C", HAG2),
        grib("2d", TD, "°C", HAG2),
        grib("2r", (0, 19, 0), "m", Some("sfc")),
    ]);
    assert!(plan.derived.is_empty());
}

/// The OGC API - EDR Metocean profile caps a parameter `label` at 50
/// characters (api-edr's `MAX_PARAMETER_LABEL_CHARS`), which cuts a longer
/// title with an ellipsis. Every derived title, with the level labels the
/// engines append where a temperature meets its humidity ingredients,
/// fits uncut and still names the saturation reference.
#[test]
fn titles_fit_the_edr_label_uncut() {
    const MAX_LABEL_CHARS: usize = 50;
    let at = |label: Option<&str>| {
        let level = |p: ParameterFacts| ParameterFacts {
            level: label.map(|_| "level".into()),
            level_label: label.map(String::from),
            vertical: None,
            ..p
        };
        let rh = build(vec![
            level(grib("TMP", T, "°C", HAG2)),
            level(grib("DPT", TD, "°C", HAG2)),
        ]);
        let td = build(vec![
            level(grib("TMP", T, "°C", HAG2)),
            level(grib("RH", RH, "%", HAG2)),
        ]);
        [rh.derived[0].title.clone(), td.derived[0].title.clone()]
    };
    // GRIB (`format_level_qualifier`), CF (`{value} {units}`), and
    // QueryData and pressure levels without one.
    for label in [
        Some("2 m above ground"),
        Some("10 m above ground"),
        Some("100 m above ground"),
        Some("entire atmosphere"),
        Some("2 m"),
        None,
    ] {
        for title in at(label) {
            assert!(
                title.chars().count() <= MAX_LABEL_CHARS,
                "{title:?}: {} characters",
                title.chars().count()
            );
        }
        assert!(at(label)[0].starts_with("Relative humidity over water"));
    }
    assert_eq!(
        at(Some("2 m above ground"))[0],
        "Relative humidity over water (2 m above ground)"
    );
}
