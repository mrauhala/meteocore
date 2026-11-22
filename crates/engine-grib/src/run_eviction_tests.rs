//! `max_runs` while a run publishes (#1562): the newest run is kept on top
//! of the newest `max_runs` runs that have finished, so its first steps add
//! to the horizon instead of replacing the run that covers it, until no
//! scan has seen a new index of it for [`PUBLISHING_GRACE`], by the
//! engine's own clock.
//!
//! The source is laid out as a remote one is, a `YYYYMMDD/HH/` folder per
//! run, and scanned as remote run prefixes are; each file's modification
//! time is set as the store would report it.

use super::*;
use crate::publication::PUBLISHING_GRACE;
use crate::test_support::{message, TestSource};
use ds_core::map_engine::default_request_time;

fn t(text: &str) -> DateTime<Utc> {
    text.parse().unwrap()
}

fn minutes(n: i64) -> chrono::Duration {
    chrono::Duration::minutes(n)
}

/// The three runs the issue saw, as ECMWF publishes them: 12Z to +48 h,
/// 18Z to +24 h, 00Z to +48 h, every 6 h, each `TMP` a constant field.
const R12: &str = "2026040512";
const R18: &str = "2026040518";
const R00: &str = "2026040600";

fn reference(run: &str) -> DateTime<Utc> {
    let date = chrono::NaiveDateTime::parse_from_str(&format!("{run}00"), "%Y%m%d%H%M").unwrap();
    date.and_utc()
}

/// Each run's own temperature, in °C as the engine serves it.
fn celsius(run: &str) -> f64 {
    match run {
        R12 => 12.0,
        R18 => 18.0,
        _ => 0.0,
    }
}

struct Runs {
    source: TestSource,
    engine: GribEngine,
    /// How far the store's clock is ahead of the engine's.
    skew: chrono::Duration,
}

impl Runs {
    fn new(max_runs: usize) -> Self {
        Self::with(max_runs, None)
    }

    /// `run_hours`: the run hours scanned, `[0, 6, 12, 18]` unless set.
    fn with(max_runs: usize, run_hours: Option<Vec<u32>>) -> Self {
        let source = TestSource::new();
        let mut config = source.config();
        config.max_runs = Some(max_runs);
        config.run_hours = run_hours;
        // Built over the empty directory, then scanned by run prefix.
        let mut engine = GribEngine::new("publishing", &config).unwrap();
        Arc::get_mut(&mut engine.source).unwrap().scan_mode = ScanMode::Remote {
            prefix_pattern: "%Y%m%d/{run}/".into(),
        };
        Self {
            source,
            engine,
            skew: chrono::Duration::zero(),
        }
    }

    /// Publish step `step` of `run` (`YYYYMMDDHH`) at `written`, by the
    /// engine's clock.
    fn publish(&self, run: &str, step: u32, written: DateTime<Utc>) {
        let folder = format!("{}/{}", &run[..8], &run[8..]);
        std::fs::create_dir_all(self.source.dir.join(&folder)).unwrap();
        let name = format!("{folder}/f{step:03}");
        let kelvin = (celsius(run) + 273.15) as f32;
        self.source.write_run(
            &name,
            run,
            &[(
                "TMP",
                "2 m above ground",
                &format!("{step} hour fcst"),
                message(0, kelvin, [0; 4], 103, 2),
            )],
        );
        for extension in ["idx", "grib2"] {
            let path = self.source.dir.join(format!("{name}.{extension}"));
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified((written + self.skew).into())
                .unwrap();
        }
    }

    /// Publish `run`'s steps `first..=last` every 6 h, one a minute from
    /// `from`.
    fn publish_run(&self, run: &str, first: u32, last: u32, from: DateTime<Utc>) {
        for (i, step) in (first..=last).step_by(6).enumerate() {
            self.publish(run, step, from + minutes(i as i64));
        }
    }

    /// 12Z and 18Z, published long before the scans.
    fn finished_runs(&self) {
        self.publish_run(R12, 0, 48, t("2026-04-05T19:00:00Z"));
        self.publish_run(R18, 0, 24, t("2026-04-06T01:00:00Z"));
    }

    fn scan(&self, now: DateTime<Utc>) {
        self.engine.scan_at(now).unwrap();
    }

    /// The next scan lists every run prefix, as the hourly re-list does.
    fn relist_all(&self) {
        *self.engine.source.last_full_scan.lock().unwrap() = None;
    }

    fn runs(&self) -> Vec<DateTime<Utc>> {
        self.engine.catalog().runs.keys().copied().collect()
    }

    /// The one value of a map at `time`, its run resolved as the API
    /// layers resolve it.
    fn map(&self, time: DateTime<Utc>) -> Result<(DateTime<Utc>, f64), DataServerError> {
        let run = self.engine.resolve_reference_time(Some(time), None);
        let tile = self.engine.get_raster_tile(
            [0.0, 0.0, 1.0, 1.0],
            1,
            1,
            Some(time),
            &OutputCrs::Wgs84,
            None,
            None,
            run,
        )?;
        let value = tile.values.value_at(0).unwrap();
        Ok((run.unwrap(), (value * 100.0).round() / 100.0))
    }

    /// The EDR position series: its times and `TMP` values.
    fn position(
        &self,
        datetime: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> (Vec<DateTime<Utc>>, Vec<Option<f64>>) {
        let CoverageResponse::Single(result) = self
            .engine
            .query_position(
                "POINT(0.5 0.5)",
                datetime.map(|t| (t, t)),
                None,
                None,
                reference_time,
            )
            .unwrap()
        else {
            panic!("expected one series")
        };
        let DomainDescription::PointSeries { t, .. } = result.domain else {
            panic!("expected a point series")
        };
        let values = result.ranges["TMP"]
            .values
            .iter()
            .map(|v| v.map(|v| (v * 100.0).round() / 100.0))
            .collect();
        (t, values)
    }
}

/// With `max_runs = 2`, the 12Z run stays while the 00Z run publishes, and
/// every API answers from the three runs alike; it goes once no scan has
/// seen a new 00Z index for the grace, with no new index to say so.
#[test]
fn the_oldest_finished_run_stays_until_the_newest_has_published() {
    let runs = Runs::new(2);
    runs.finished_runs();
    runs.scan(t("2026-04-06T07:30:00Z"));
    assert_eq!(runs.runs(), [reference(R12), reference(R18)]);

    // The 00Z run's analysis.
    runs.publish(R00, 0, t("2026-04-06T07:41:00Z"));
    runs.scan(t("2026-04-06T07:42:00Z"));
    let three = [reference(R12), reference(R18), reference(R00)];
    assert_eq!(runs.runs(), three, "12Z is kept while 00Z publishes");

    // Instances, the map axes and defaults, and EDR agree on them.
    let engine = &runs.engine;
    let instances: Vec<_> = engine
        .get_instances()
        .iter()
        .map(|r| r.reference_time)
        .collect();
    assert_eq!(instances, three);
    assert_eq!(engine.instance_reference_times(), three);
    assert_eq!(engine.latest_reference_time(), Some(reference(R00)));
    let info = engine.raster_info_shared();
    assert_eq!(info.reference_times, three);
    let horizon = reference(R12) + chrono::Duration::hours(48);
    assert_eq!(info.times.last(), Some(&horizon));
    assert_eq!(default_request_time(engine, &info, None), Some(horizon));
    assert_eq!(
        engine.run_times(None, reference(R12)).map(|t| t.len()),
        Some(9)
    );
    // Past 18Z's last step only 12Z covers: maps and EDR read it there.
    assert_eq!(runs.map(horizon).unwrap(), (reference(R12), celsius(R12)));
    assert_eq!(
        runs.position(Some(horizon), None),
        (vec![horizon], vec![Some(celsius(R12))])
    );
    assert_eq!(
        runs.position(Some(horizon), Some(reference(R12))),
        (vec![horizon], vec![Some(celsius(R12))])
    );
    // At 00Z's analysis the newest run draws; without a datetime EDR reads
    // the latest run, 00Z, as before.
    assert_eq!(
        runs.map(reference(R00)).unwrap(),
        (reference(R00), celsius(R00))
    );
    assert_eq!(
        runs.position(None, None),
        (vec![reference(R00)], vec![Some(celsius(R00))])
    );

    // The rest of 00Z, one step a minute, seen by the 07:55 scan; then a
    // scan inside the grace.
    runs.publish_run(R00, 6, 48, t("2026-04-06T07:43:00Z"));
    let last_seen = t("2026-04-06T07:55:00Z");
    runs.scan(last_seen);
    assert_eq!(runs.runs(), three);
    runs.scan(last_seen + PUBLISHING_GRACE - chrono::Duration::seconds(1));
    assert_eq!(runs.runs(), three, "still inside the grace");

    // The grace has passed: 12Z goes, with no new index.
    runs.scan(last_seen + PUBLISHING_GRACE);
    let two = [reference(R18), reference(R00)];
    assert_eq!(runs.runs(), two);
    assert_eq!(runs.engine.instance_reference_times(), two);
    let info = runs.engine.raster_info_shared();
    assert_eq!(info.reference_times, two);
    let horizon = reference(R00) + chrono::Duration::hours(48);
    assert_eq!(
        default_request_time(&runs.engine, &info, None),
        Some(horizon)
    );
    assert_eq!(runs.map(horizon).unwrap(), (reference(R00), celsius(R00)));
}

/// A store whose clock is three hours ahead changes nothing: its times only
/// tell a rewritten file, and the grace runs on the engine's clock.
#[test]
fn a_store_clock_ahead_does_not_extend_the_grace() {
    let mut runs = Runs::new(2);
    runs.skew = chrono::Duration::hours(3);
    runs.finished_runs();
    runs.scan(t("2026-04-06T07:30:00Z"));
    runs.publish(R00, 0, t("2026-04-06T07:41:00Z"));
    let seen = t("2026-04-06T07:42:00Z");
    runs.scan(seen);
    assert_eq!(runs.runs().len(), 3);
    runs.scan(seen + PUBLISHING_GRACE - chrono::Duration::seconds(1));
    assert_eq!(runs.runs().len(), 3);
    runs.scan(seen + PUBLISHING_GRACE);
    assert_eq!(runs.runs(), [reference(R18), reference(R00)]);
}

/// A run whose publication stops keeps the extra run only for the grace.
#[test]
fn a_run_that_stops_publishing_keeps_the_extra_run_only_for_the_grace() {
    let runs = Runs::new(2);
    runs.finished_runs();
    runs.scan(t("2026-04-06T07:30:00Z"));
    runs.publish(R00, 0, t("2026-04-06T07:41:00Z"));
    runs.publish(R00, 6, t("2026-04-06T07:41:00Z"));
    let seen = t("2026-04-06T07:42:00Z");
    runs.scan(seen);
    assert_eq!(runs.runs().len(), 3);

    // No more 00Z files, and a listing that finds none meanwhile: the
    // grace neither restarts nor ends.
    let folder = runs.source.dir.join("20260406");
    let away = runs.source.dir.join("20260406-away");
    std::fs::rename(&folder, &away).unwrap();
    runs.scan(seen + minutes(20));
    std::fs::rename(&away, &folder).unwrap();
    assert_eq!(runs.runs().len(), 3);
    runs.scan(seen + PUBLISHING_GRACE - chrono::Duration::seconds(1));
    assert_eq!(runs.runs().len(), 3);

    runs.scan(seen + PUBLISHING_GRACE);
    assert_eq!(runs.runs(), [reference(R18), reference(R00)]);
    let times = runs.engine.run_times(None, reference(R00)).unwrap();
    assert_eq!(times.len(), 2, "the stalled run serves what it has");
}

/// Once the newest run has finished, an index of it rewritten, or a stalled
/// run's late step, does not make it publish again: the hourly re-list
/// does not read back the run already evicted.
#[test]
fn a_finished_run_rewritten_does_not_bring_back_the_evicted_run() {
    let runs = Runs::new(2);
    runs.finished_runs();
    runs.scan(t("2026-04-06T07:30:00Z"));
    runs.publish_run(R00, 0, 42, t("2026-04-06T07:41:00Z"));
    let seen = t("2026-04-06T07:50:00Z");
    runs.scan(seen);
    runs.scan(seen + PUBLISHING_GRACE);
    let two = [reference(R18), reference(R00)];
    assert_eq!(runs.runs(), two);

    // Its analysis written again, and its last step, late.
    let later = seen + PUBLISHING_GRACE + minutes(5);
    runs.publish(R00, 0, later - minutes(1));
    runs.publish(R00, 48, later - minutes(1));
    runs.relist_all();
    runs.scan(later);
    assert_eq!(runs.runs(), two, "12Z is not read back");
    assert_eq!(
        runs.engine.run_times(None, reference(R00)).unwrap().len(),
        9
    );
    // Nor by the next re-list, within the grace of the rewrite.
    runs.relist_all();
    runs.scan(later + minutes(1));
    assert_eq!(runs.runs(), two, "12Z is not read back later");
    let known = runs.engine.source.known_indexes.lock().unwrap();
    assert!(
        !known.iter().any(|path| path.starts_with("20260405/12")),
        "{known:?}"
    );
}

/// In one directory every run's files are listed together: an older run's
/// file written again does not make the newest run publish, nor keep the
/// older run on top of `max_runs`.
#[test]
fn an_older_runs_file_written_again_keeps_no_extra_run() {
    let source = TestSource::new();
    let rewrite = |run: &str, written: Option<DateTime<Utc>>| {
        let name = format!("r{run}");
        let record = (
            "TMP",
            "2 m above ground",
            "anl",
            message(0, 280.0, [0; 4], 103, 2),
        );
        source.write_run(&name, run, &[record]);
        for extension in written.map_or(&[][..], |_| &["idx", "grib2"][..]) {
            std::fs::File::options()
                .write(true)
                .open(source.dir.join(format!("{name}.{extension}")))
                .unwrap()
                .set_modified(written.unwrap().into())
                .unwrap();
        }
    };
    rewrite("2026040606", None);
    rewrite("2026040607", None);
    let mut config = source.config();
    config.max_runs = Some(1);
    // A restart: 07Z might still publish.
    let engine = GribEngine::new("one-directory", &config).unwrap();
    let start = Utc::now();
    let runs = |engine: &GribEngine| engine.catalog().runs.keys().copied().collect::<Vec<_>>();
    let both = [reference("2026040606"), reference("2026040607")];
    assert_eq!(runs(&engine), both);

    // A sync writes the 06Z file again while 07Z publishes: not 07Z's.
    rewrite("2026040606", Some(start + minutes(19)));
    engine.scan_at(start + minutes(20)).unwrap();
    assert_eq!(runs(&engine), both);
    engine.scan_at(start + PUBLISHING_GRACE).unwrap();
    assert_eq!(runs(&engine), [reference("2026040607")]);

    // And again once 07Z has finished.
    let rewritten = start + PUBLISHING_GRACE + minutes(1);
    rewrite("2026040606", Some(rewritten));
    engine.scan_at(rewritten + minutes(1)).unwrap();
    assert_eq!(runs(&engine), [reference("2026040607")]);
}

/// Hourly runs published back to back, each starting before the one below
/// it has finished: `max_runs + 1` runs throughout, the one kept on top
/// always the newest, the other the run just below it.
#[test]
fn back_to_back_runs_keep_the_run_just_below_the_newest() {
    let runs = Runs::with(1, Some((0..24).collect()));
    let hour = |h: u32| format!("20260406{h:02}");
    runs.publish_run(&hour(5), 0, 12, t("2026-04-06T05:40:00Z"));
    runs.scan(t("2026-04-06T05:45:00Z"));
    runs.scan(t("2026-04-06T06:20:00Z"));
    assert_eq!(runs.runs(), [reference(&hour(5))]);

    for h in 6..=8 {
        let start = reference(&hour(h)) + minutes(40);
        runs.publish(&hour(h), 0, start);
        runs.scan(start + minutes(1));
        assert_eq!(
            runs.runs(),
            [reference(&hour(h - 1)), reference(&hour(h))],
            "{h}Z publishes"
        );
        // Its other steps, and the next run's analysis within the grace.
        runs.publish_run(&hour(h), 6, 12, start + minutes(10));
        runs.scan(start + minutes(15));
        assert_eq!(runs.runs().len(), 2);
    }
    // The last one finishes.
    runs.scan(reference(&hour(8)) + minutes(55) + PUBLISHING_GRACE);
    assert_eq!(runs.runs(), [reference(&hour(8))]);
}

/// A restart cannot tell whether the newest run still publishes: it lists
/// one more run and keeps it for the grace.
#[test]
fn a_restart_lists_one_more_run_and_keeps_it_for_the_grace() {
    // While 00Z publishes: the run it replaces is found too.
    let runs = Runs::new(2);
    runs.finished_runs();
    runs.publish_run(R00, 0, 12, t("2026-04-06T07:41:00Z"));
    let restart = t("2026-04-06T07:45:00Z");
    runs.scan(restart);
    let three = [reference(R12), reference(R18), reference(R00)];
    assert_eq!(runs.runs(), three);
    runs.publish_run(R00, 18, 48, t("2026-04-06T07:46:00Z"));
    let seen = t("2026-04-06T07:55:00Z");
    runs.scan(seen);
    runs.scan(seen + PUBLISHING_GRACE - chrono::Duration::seconds(1));
    assert_eq!(runs.runs(), three);
    runs.scan(seen + PUBLISHING_GRACE);
    assert_eq!(runs.runs(), [reference(R18), reference(R00)]);

    // Long after: the extra run is kept for the grace from the restart.
    let runs = Runs::new(2);
    runs.finished_runs();
    runs.publish_run(R00, 0, 48, t("2026-04-06T07:41:00Z"));
    let restart = t("2026-04-06T12:00:00Z");
    runs.scan(restart);
    assert_eq!(runs.runs(), three);
    runs.scan(restart + PUBLISHING_GRACE);
    assert_eq!(runs.runs(), [reference(R18), reference(R00)]);
}
