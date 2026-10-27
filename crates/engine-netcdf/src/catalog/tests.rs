use std::path::PathBuf;

use chrono::TimeZone;

use super::*;

/// A variable of `units` and `standard_name`, on one level.
fn variable(name: &str, units: &str, standard_name: Option<&str>) -> Variable {
    Variable {
        name: name.into(),
        title: name.into(),
        units: units.into(),
        standard_name: standard_name.map(str::to_string),
        levels: Vec::new(),
        pressure: None,
    }
}

/// A 2 × 2 one-degree grid.
fn grid() -> FileGrid {
    FileGrid::Regular {
        transform: ds_core::geo::GeoTransform {
            origin_x: 0.0,
            origin_y: 2.0,
            pixel_width: 1.0,
            pixel_height: -1.0,
            width: 2,
            height: 2,
            crs: Crs::Wgs84,
        },
        flipped: false,
    }
}

/// A file to hang steps on: selection never reads it.
fn file() -> Arc<OpenFile> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/netcdf-global/global_5deg.nc");
    let reader = crate::ingest::open(&path).unwrap();
    Arc::new(OpenFile {
        id: 0,
        path,
        identity: Identity::sized(0),
        reader,
        run: None,
        times: Vec::new(),
        reads: Vec::new(),
        heap_bytes: 0,
    })
}

/// Hour `h` of 2026-10-15.
fn t(h: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 15, 0, 0, 0).unwrap() + chrono::Duration::hours(h)
}

/// A catalog of `(run hour, step hours)`; `None` is the run-less timeline.
fn catalog(runs: &[(Option<i64>, &[i64])]) -> Catalog {
    let file = file();
    let mut catalog = Catalog::empty(1);
    for &(run, steps) in runs {
        let reference_time = run.map(t);
        catalog.runs.insert(
            reference_time,
            Run {
                reference_time,
                steps: steps
                    .iter()
                    .map(|&h| {
                        let step = Step {
                            file: file.clone(),
                            index: h as u32,
                        };
                        (t(h), step)
                    })
                    .collect(),
            },
        );
    }
    catalog
}

/// The run hour and valid hour `select` chooses for a pinned run hour and
/// a time in hours.
fn chosen(
    catalog: &Catalog,
    run: Option<i64>,
    time: Option<f64>,
) -> Result<(Option<i64>, f64), String> {
    let time = time.map(|h| t(0) + chrono::Duration::minutes((h * 60.0) as i64));
    let hours = |at: DateTime<Utc>| (at - t(0)).num_minutes() as f64 / 60.0;
    catalog
        .select(run.map(t), time)
        .map(|s| (s.reference_time.map(|r| hours(r) as i64), hours(s.valid)))
        .map_err(|e| e.to_string())
}

/// The fixture's two MEPS runs: 06Z leads 0-3 and 09Z leads 0-2, so 09Z
/// is in both.
fn meps() -> Catalog {
    catalog(&[(Some(6), &[6, 7, 8, 9]), (Some(9), &[9, 10, 11])])
}

#[test]
fn unpinned_takes_the_newest_run_covering_the_time() {
    let catalog = meps();
    assert_eq!(chosen(&catalog, None, Some(9.0)), Ok((Some(9), 9.0)));
    assert_eq!(chosen(&catalog, None, Some(10.4)), Ok((Some(9), 10.0)));
    // The newest run starts later: the older one still answers.
    assert_eq!(chosen(&catalog, None, Some(8.0)), Ok((Some(6), 8.0)));
    assert_eq!(chosen(&catalog, None, Some(7.75)), Ok((Some(6), 8.0)));
    // Nearest step, the earlier on a tie.
    assert_eq!(chosen(&catalog, None, Some(6.5)), Ok((Some(6), 6.0)));
    // No time: the newest run's last step.
    assert_eq!(chosen(&catalog, None, None), Ok((Some(9), 11.0)));
    for outside in [5.0, 11.5] {
        let error = chosen(&catalog, None, Some(outside)).unwrap_err();
        assert!(error.contains("No forecast run covers"), "{error}");
    }
}

#[test]
fn a_pinned_run_never_falls_back() {
    let catalog = meps();
    assert_eq!(chosen(&catalog, Some(6), Some(9.0)), Ok((Some(6), 9.0)));
    assert_eq!(chosen(&catalog, Some(6), None), Ok((Some(6), 9.0)));
    let error = chosen(&catalog, Some(6), Some(10.0)).unwrap_err();
    assert!(error.contains("No forecast step"), "{error}");
    let error = chosen(&catalog, Some(9), Some(8.0)).unwrap_err();
    assert!(error.contains("No forecast step"), "{error}");
    assert!(matches!(
        catalog.select(Some(t(3)), Some(t(6))),
        Err(DataServerError::ReferenceTimeNotFound(_))
    ));
}

/// The API layers resolve the run, then pin it for the time and the
/// render (#521): pinning what the unpinned selection chose, at the time
/// or at the valid time it drew, draws the same step.
#[test]
fn pinning_the_chosen_run_chooses_the_same_step() {
    let catalog = meps();
    for quarter in 24..=44 {
        let time = Some(f64::from(quarter) / 4.0);
        let Ok((run, valid)) = chosen(&catalog, None, time) else {
            continue;
        };
        assert_eq!(chosen(&catalog, run, time), Ok((run, valid)), "{time:?}");
        assert_eq!(
            chosen(&catalog, run, Some(valid)),
            Ok((run, valid)),
            "{time:?}"
        );
    }
}

#[test]
fn files_without_a_run_form_one_timeline() {
    let catalog = catalog(&[(None, &[0, 6, 12])]);
    assert_eq!(chosen(&catalog, None, Some(5.0)), Ok((None, 6.0)));
    assert_eq!(chosen(&catalog, None, None), Ok((None, 12.0)));
    assert!(matches!(
        catalog.select(Some(t(0)), None),
        Err(DataServerError::ReferenceTimeNotFound(_))
    ));
    assert!(Catalog::empty(1).select(None, None).is_err());
}

/// CF's unit `1` is served in % only where the variable's `standard_name`
/// names a fraction (relative humidity, an area fraction); the variable's
/// own name decides nothing. A mass fraction, a flag, or a variable
/// without a standard name keeps `1` and its stored values.
#[test]
fn unit_1_is_percent_only_for_a_fraction_standard_name() {
    let layout = Layout::new(
        vec![
            variable("relative_humidity_2m", "1", Some("relative_humidity")),
            variable("cloud_area_fraction", "1", Some("cloud_area_fraction")),
            variable("lcc", "1", Some("low_type_cloud_area_fraction")),
            variable("rh_percent", "%", Some("relative_humidity")),
            variable("specific_humidity_2m", "1", Some("specific_humidity")),
            variable("land_binary_mask", "1", Some("land_binary_mask")),
            variable("relative_humidity", "1", None),
            variable("air_temperature_2m", "K", Some("air_temperature")),
        ],
        grid(),
        true,
    );
    let served: Vec<(&str, f64)> = (0..layout.variables.len())
        .map(|i| (layout.unit(i), layout.to_display(i, 0.5)))
        .collect();
    assert_eq!(
        served,
        [
            ("%", 50.0),
            ("%", 50.0),
            ("%", 50.0),
            ("%", 0.5),
            ("1", 0.5),
            ("1", 0.5),
            ("1", 0.5),
            ("°C", 0.5 - 273.15),
        ]
    );
    // The wind facts report the served unit too.
    let units: Vec<&str> = layout
        .wind
        .parameters
        .iter()
        .map(|p| p.unit.as_str())
        .collect();
    assert_eq!(units, ["%", "%", "%", "%", "1", "1", "1", "°C"]);
}

/// #1165: a variable named `…_acc` served in mm (kg m-2 or mm, in any
/// spelling) gives a per-step amount named without the `_acc`, after the
/// variables, titled from its standard name; an `_acc` in another unit, or
/// one whose per-step name a variable already has, gives none, nor does
/// any with `derive_per_step = false`.
#[test]
fn run_start_accumulations_in_mm_give_per_step_amounts() {
    let variables = vec![
        variable("air_temperature_2m", "K", Some("air_temperature")),
        variable(
            "precipitation_amount_acc",
            "kg/m^2",
            Some("precipitation_amount"),
        ),
        variable("snowfall_amount_acc", "mm", None),
        // Signed: a step's amount clamped at zero would be wrong.
        variable(
            "integral_of_surface_net_downward_longwave_flux_wrt_time_acc",
            "J m-2",
            None,
        ),
        // The file serves its own per-step amount: that one is served.
        variable("graupelfall_amount_acc", "kg m-2", None),
        variable("graupelfall_amount", "kg m-2", None),
        variable("_acc", "kg m-2", None),
    ];
    let layout = Layout::new(variables.clone(), grid(), true);
    let derived: Vec<(&str, &str, &str, &str)> = layout
        .per_step
        .iter()
        .map(|p| {
            (
                &*p.name,
                &*layout.variables[p.source].name,
                p.label.as_str(),
                layout.unit(p.source),
            )
        })
        .collect();
    assert_eq!(
        derived,
        [
            (
                "precipitation_amount",
                "precipitation_amount_acc",
                "Precipitation amount",
                "mm"
            ),
            (
                "snowfall_amount",
                "snowfall_amount_acc",
                "Snowfall amount",
                "mm"
            ),
        ]
    );
    // Served after the variables, by name, reading their accumulation.
    let n = variables.len();
    let params: Vec<Param> = layout.params().collect();
    assert_eq!(params.len(), n + 2);
    let precipitation = layout.param("precipitation_amount").unwrap();
    assert_eq!(precipitation, Param::PerStep(0));
    assert_eq!(
        (
            layout.source_of(precipitation),
            layout.info_index(precipitation),
            layout.name(precipitation)
        ),
        (1, n, "precipitation_amount")
    );
    assert_eq!(
        layout.param("precipitation_amount_acc"),
        Some(Param::Stored(1))
    );
    assert_eq!(layout.param("rain"), None);
    assert!(Layout::new(variables, grid(), false).per_step.is_empty());
}

/// #1165: a step's amount is the accumulation less the step before's,
/// clamped at zero; a run's first step is its own accumulation; a missing
/// sample at either step is no value.
#[test]
fn a_step_amount_is_the_clamped_difference() {
    assert_eq!(step_amount(Some(1.5), Some(Some(0.5))), Some(1.0));
    assert_eq!(step_amount(Some(0.25), None), Some(0.25), "the first step");
    assert_eq!(
        step_amount(Some(0.004), Some(Some(0.0045))),
        Some(0.0),
        "a decreasing accumulation is no amount"
    );
    assert_eq!(step_amount(Some(-0.1), None), Some(0.0));
    assert_eq!(step_amount(None, Some(Some(0.5))), None);
    assert_eq!(step_amount(Some(1.0), Some(None)), None);
}

/// #1165: the title says the step a per-step amount spans, from the runs
/// held: between consecutive steps, and from a run's reference time to a
/// first step after it. One length is GRIB's `(1 h accumulation)`; several,
/// as when a run lacks a lead, the shortest and longest; none, a run of one
/// step at its reference time, no length.
#[test]
fn the_title_says_the_step_length() {
    let title = |runs: &[(Option<i64>, &[i64])]| {
        per_step_title("Precipitation amount", &step_lengths(&catalog(runs).runs))
    };
    assert_eq!(
        title(&[(Some(6), &[6, 7, 8, 9]), (Some(9), &[9, 10, 11])]),
        "Precipitation amount (1 h accumulation)"
    );
    // A run starting at its first lead after the reference time.
    assert_eq!(
        title(&[(Some(0), &[1, 2])]),
        "Precipitation amount (1 h accumulation)"
    );
    assert_eq!(
        title(&[(Some(6), &[6, 7, 9])]),
        "Precipitation amount (1 h to 2 h accumulation per forecast step)"
    );
    assert_eq!(
        title(&[(None, &[0, 6, 12])]),
        "Precipitation amount (6 h accumulation)"
    );
    assert_eq!(
        title(&[(Some(6), &[6])]),
        "Precipitation amount (accumulation per forecast step)"
    );
    assert_eq!(step_words(chrono::Duration::minutes(15)), "15 min");
    assert_eq!(step_words(chrono::Duration::seconds(90)), "90 s");
}

/// #1165: the step before each step of a run, through `select` as the map
/// uses it; none at the run's first.
#[test]
fn a_selected_step_names_the_step_before() {
    let catalog = meps();
    let before = |run: Option<i64>, time: i64| {
        let selected = catalog.select(run.map(t), Some(t(time))).unwrap();
        selected.previous.map(|step| step.index)
    };
    assert_eq!(before(Some(6), 6), None, "the run's first step");
    assert_eq!(before(Some(6), 8), Some(7));
    assert_eq!(before(Some(9), 9), None, "09Z's first, not 06Z's 08Z");
    assert_eq!(before(None, 11), Some(10));
}

/// #1165: a lead filled in, or removed, inside a run changes the step
/// before a step served before; a lead appended after the run's last, or a
/// run added, does not.
#[test]
fn a_lead_inside_a_run_changes_the_step_before() {
    let changed = |old: &[(Option<i64>, &[i64])], new: &[(Option<i64>, &[i64])]| {
        steps_before_changed(&catalog(old).runs, &catalog(new).runs)
    };
    assert!(!changed(&[(Some(6), &[6, 7])], &[(Some(6), &[6, 7, 8])]));
    assert!(!changed(
        &[(Some(6), &[6, 7])],
        &[(Some(6), &[6, 7]), (Some(9), &[9, 10])]
    ));
    assert!(changed(
        &[(Some(6), &[6, 7, 9])],
        &[(Some(6), &[6, 7, 8, 9])]
    ));
    assert!(changed(&[(Some(6), &[6, 7, 8])], &[(Some(6), &[6, 8])]));
    assert!(changed(&[(Some(6), &[7, 8])], &[(Some(6), &[6, 7, 8])]));
    assert!(!changed(&[], &[(Some(6), &[6, 7, 8])]));
}
