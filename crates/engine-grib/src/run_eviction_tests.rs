//! `max_runs` while a run publishes (#1562): the newest run is kept on top
//! of the newest `max_runs` runs that have finished, so its first steps add
//! to the horizon instead of replacing the run that covers it, until no
//! index of it has been written for [`PUBLISHING_GRACE`].
//!
//! The source is laid out as a remote one is, a `YYYYMMDD/HH/` folder per
//! run, and scanned as remote run prefixes are; each file's modification
//! time is when it was published.

use super::*;
use crate::test_support::{message, TestSource};
use ds_core::map_engine::default_request_time;

fn t(text: &str) -> DateTime<Utc> {
    text.parse().unwrap()
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
}

impl Runs {
    fn new(max_runs: usize) -> Self {
        let source = TestSource::new();
        let mut config = source.config();
        config.max_runs = Some(max_runs);
        // Built over the empty directory, then scanned by run prefix.
        let mut engine = GribEngine::new("publishing", &config).unwrap();
        Arc::get_mut(&mut engine.source).unwrap().scan_mode = ScanMode::Remote {
            prefix_pattern: "%Y%m%d/{run}/".into(),
        };
        Self { source, engine }
    }

    /// Publish step `step` of `run` (`YYYYMMDDHH`) at `written`.
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
                .set_modified(written.into())
                .unwrap();
        }
    }

    /// Publish `run`'s steps `first..=last` every 6 h, one a minute from
    /// `from`; when the last was written.
    fn publish_run(&self, run: &str, first: u32, last: u32, from: DateTime<Utc>) -> DateTime<Utc> {
        let mut written = from;
        for (i, step) in (first..=last).step_by(6).enumerate() {
            written = from + chrono::Duration::minutes(i as i64);
            self.publish(run, step, written);
        }
        written
    }

    /// 12Z and 18Z, published long before `now`.
    fn finished_runs(&self) {
        self.publish_run(R12, 0, 48, t("2026-04-05T19:00:00Z"));
        self.publish_run(R18, 0, 24, t("2026-04-06T01:00:00Z"));
    }

    fn scan(&self, now: &str) {
        self.engine.scan_at(t(now)).unwrap();
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
/// every API answers from the three runs alike; it goes once no 00Z index
/// has been written for the grace, with no new index to say so.
#[test]
fn the_oldest_finished_run_stays_until_the_newest_has_published() {
    let runs = Runs::new(2);
    runs.finished_runs();
    runs.scan("2026-04-06T07:30:00Z");
    assert_eq!(runs.runs(), [reference(R12), reference(R18)]);

    // The 00Z run's analysis.
    runs.publish(R00, 0, t("2026-04-06T07:41:00Z"));
    runs.scan("2026-04-06T07:42:00Z");
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

    // The rest of 00Z, one step a minute; then a poll inside the grace.
    let last = runs.publish_run(R00, 6, 48, t("2026-04-06T07:43:00Z"));
    runs.scan("2026-04-06T07:55:00Z");
    assert_eq!(runs.runs(), three);
    runs.scan(&(last + PUBLISHING_GRACE - chrono::Duration::minutes(1)).to_rfc3339());
    assert_eq!(runs.runs(), three, "still inside the grace");

    // The grace has passed: 12Z goes, with no new index.
    runs.scan(&(last + PUBLISHING_GRACE).to_rfc3339());
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

/// A run whose publication stops keeps the extra run only for the grace.
#[test]
fn a_run_that_stops_publishing_keeps_the_extra_run_only_for_the_grace() {
    let runs = Runs::new(2);
    runs.finished_runs();
    runs.scan("2026-04-06T07:30:00Z");
    let analysis = t("2026-04-06T07:41:00Z");
    runs.publish(R00, 0, analysis);
    runs.publish(R00, 6, analysis);
    runs.scan("2026-04-06T07:42:00Z");
    assert_eq!(runs.runs().len(), 3);

    // No more 00Z files, and a listing that fails meanwhile: the grace
    // neither restarts nor ends.
    let missing = runs.source.dir.join("20260406");
    let hidden = runs.source.dir.join("20260406-away");
    std::fs::rename(&missing, &hidden).unwrap();
    runs.scan(&(analysis + chrono::Duration::minutes(20)).to_rfc3339());
    std::fs::rename(&hidden, &missing).unwrap();
    assert_eq!(runs.runs().len(), 3);
    runs.scan(&(analysis + PUBLISHING_GRACE - chrono::Duration::seconds(1)).to_rfc3339());
    assert_eq!(runs.runs().len(), 3);

    runs.scan(&(analysis + PUBLISHING_GRACE).to_rfc3339());
    assert_eq!(runs.runs(), [reference(R18), reference(R00)]);
    let times = runs.engine.run_times(None, reference(R00)).unwrap();
    assert_eq!(times.len(), 2, "the stalled run serves what it has");
}

/// A restart while a run publishes lists one more run, the one the new run
/// is about to replace; one after it lists `max_runs` runs only.
#[test]
fn a_restart_while_a_run_publishes_lists_the_run_it_replaces() {
    let runs = Runs::new(2);
    runs.finished_runs();
    runs.publish_run(R00, 0, 12, t("2026-04-06T07:41:00Z"));
    runs.scan("2026-04-06T07:45:00Z");
    assert_eq!(
        runs.runs(),
        [reference(R12), reference(R18), reference(R00)]
    );

    let runs = Runs::new(2);
    runs.finished_runs();
    runs.publish_run(R00, 0, 48, t("2026-04-06T07:41:00Z"));
    runs.scan("2026-04-06T09:00:00Z");
    assert_eq!(runs.runs(), [reference(R18), reference(R00)]);
    let settled = runs.engine.source.settled_prefixes.lock().unwrap();
    assert!(
        !settled.contains("20260405/12"),
        "12Z was not listed: {settled:?}"
    );
    assert!(settled.contains("20260405/18"), "{settled:?}");
}
