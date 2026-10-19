use std::path::PathBuf;

use chrono::TimeZone;

use super::*;

/// A file to hang steps on: selection never reads it.
fn file() -> Arc<OpenFile> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/netcdf-global/global_5deg.nc");
    let (nc, _) = crate::ingest::open(&path).unwrap();
    Arc::new(OpenFile {
        id: 0,
        path,
        identity: Identity::sized(0),
        nc,
        remote: None,
        run: None,
        times: Vec::new(),
        reads: Vec::new(),
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
