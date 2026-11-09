//! The engine over a synthetic source whose frames are a closed-form
//! function of time and cell: every product is checked against a direct
//! `f64` computation from the same frames, through the public `MapEngine`
//! and `EdrEngine` surfaces the API layers use.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use chrono::{DateTime, Duration, TimeZone, Utc};
use ds_core::config::{PeriodConfig, RainRateConfig};
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile, RasterValues};
use ds_core::model::{CoverageResponse, DomainDescription};
use engine_period::PeriodEngine;

const NX: u32 = 32;
/// Odd, so strips of four rows end in a strip of one.
const NY: u32 = 17;
const NORTH: f64 = 67.0;
/// One-degree cells.
const EXTENT: [f64; 4] = [0.0, 50.0, 32.0, NORTH];
const CELLS: usize = (NX * NY) as usize;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap()
}

fn step(i: i64) -> DateTime<Utc> {
    t0() + Duration::minutes(5 * i)
}

/// Reflectivity of a frame at a cell: a deterministic hash over −10..60
/// dBZ, nodata at every 17th cell always and at one cell per frame.
fn dbz(t: DateTime<Utc>, cell: usize) -> Option<f64> {
    let k = (t - t0()).num_minutes() as u64;
    if cell.is_multiple_of(17) || cell == (k as usize * 7) % CELLS {
        return None;
    }
    let mut x =
        k.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (cell as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 31;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 29;
    Some(-10.0 + (x % 7001) as f64 / 100.0)
}

/// A radar-like source: a mutable frame catalog, frames from [`dbz`] as
/// raw bytes (0.5 dB steps, offset −32, 255 nodata) or `f32`. Like a real
/// source it places each output row by the latitude of its centre, so a
/// strip read gives the grid rows its box covers; it counts frame reads
/// (reads from the top row), strip reads and every grid row each frame
/// had read.
struct Source {
    times: RwLock<Vec<DateTime<Utc>>>,
    raw: bool,
    reads: AtomicUsize,
    strip_reads: AtomicUsize,
    rows_read: Mutex<HashMap<DateTime<Utc>, Vec<u32>>>,
    version: AtomicU64,
    /// Frames whose read fails, from this row on.
    broken: Mutex<Vec<(DateTime<Utc>, u32)>>,
    /// Called with each strip read's frame and first row, before it is
    /// answered: a request arriving while a poll folds strips in.
    #[allow(clippy::type_complexity)]
    hook: Mutex<Option<Box<dyn Fn(DateTime<Utc>, u32) + Send + Sync>>>,
    /// Frames revised in place: every cell has a value, 3 dB more.
    revised: Mutex<BTreeSet<DateTime<Utc>>>,
    /// The source reports no grid.
    lost_grid: AtomicBool,
}

impl Source {
    fn new(times: Vec<DateTime<Utc>>, raw: bool) -> Arc<Self> {
        Arc::new(Self {
            times: RwLock::new(times),
            raw,
            reads: AtomicUsize::new(0),
            strip_reads: AtomicUsize::new(0),
            rows_read: Mutex::new(HashMap::new()),
            version: AtomicU64::new(0),
            broken: Mutex::new(Vec::new()),
            hook: Mutex::new(None),
            revised: Mutex::new(BTreeSet::new()),
            lost_grid: AtomicBool::new(false),
        })
    }

    fn set_times(&self, times: Vec<DateTime<Utc>>) {
        *self.times.write().unwrap() = times;
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    /// The reflectivity a frame holds at a cell now, revisions included.
    fn dbz(&self, t: DateTime<Utc>, cell: usize) -> Option<f64> {
        if self.revised.lock().unwrap().contains(&t) {
            return Some(dbz(t, cell).unwrap_or(20.0) + 3.0);
        }
        dbz(t, cell)
    }

    /// The value a frame holds at a cell, as the engine reads it.
    fn value(&self, t: DateTime<Utc>, cell: usize) -> Option<f64> {
        let v = self.dbz(t, cell)?;
        Some(if self.raw {
            ((v + 32.0) / 0.5).round() * 0.5 - 32.0
        } else {
            v as f32 as f64
        })
    }
}

impl MapEngine for Source {
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        _parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        assert_eq!(
            (bbox[0], bbox[2], width),
            (EXTENT[0], EXTENT[2], NX),
            "reads whole rows of the native grid"
        );
        assert!(matches!(output_crs, OutputCrs::Wgs84));
        let t = time.expect("a frame time");
        let rows: Vec<u32> = (0..height)
            .map(|j| {
                let lat = bbox[3] - (f64::from(j) + 0.5) / f64::from(height) * (bbox[3] - bbox[1]);
                let row = (NORTH - lat).floor();
                assert!(
                    (0.0..f64::from(NY)).contains(&row),
                    "row {row} off the grid"
                );
                row as u32
            })
            .collect();
        if rows[0] == 0 {
            self.reads.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(hook) = self.hook.lock().unwrap().as_ref() {
            hook(t, rows[0]);
        }
        self.strip_reads.fetch_add(1, Ordering::SeqCst);
        if let Some(&(_, from)) = self.broken.lock().unwrap().iter().find(|(b, _)| *b == t) {
            if rows[0] >= from {
                return Err(DataServerError::Engine("unreadable".into()));
            }
        }
        {
            let mut read = self.rows_read.lock().unwrap();
            let counts = read.entry(t).or_insert_with(|| vec![0; NY as usize]);
            for &row in &rows {
                counts[row as usize] += 1;
            }
        }
        // Collected first, so the tile's buffer is exactly its size.
        let cells: Vec<usize> = rows
            .iter()
            .flat_map(|&row| (0..NX as usize).map(move |col| row as usize * NX as usize + col))
            .collect();
        let cells = cells.into_iter();
        let values = if self.raw {
            RasterValues::U8 {
                data: cells
                    .map(|c| {
                        self.dbz(t, c)
                            .map_or(255, |v| ((v + 32.0) / 0.5).round() as u8)
                    })
                    .collect(),
                nodata: Some(255),
                gain: 0.5,
                offset: -32.0,
            }
        } else {
            RasterValues::F32 {
                data: cells
                    .map(|c| self.dbz(t, c).map_or(-999.0, |v| v as f32))
                    .collect(),
                nodata: Some(-999.0),
            }
        };
        Ok(RasterTile {
            width,
            height,
            values,
        })
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: (!self.lost_grid.load(Ordering::SeqCst)).then_some(EXTENT),
            times: self.times.read().unwrap().clone(),
            parameter: "reflectivity".into(),
            unit: "dBZ".into(),
            parameters: Vec::new(),
            vertical: None,
            grid_size: Some([NX, NY]),
            layer_subtitle: None,
            reference_times: Vec::new(),
            native_grid: None,
        }
    }

    fn resolve_time(
        &self,
        time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let times = self.times.read().unwrap();
        match time {
            None => times.last().copied(),
            Some(t) => times.iter().rev().find(|&&x| x <= t).copied(),
        }
    }

    fn content_version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }
}

fn config(statistic: &str, windows: &[&str]) -> PeriodConfig {
    PeriodConfig {
        source: "radar".into(),
        parameter: None,
        statistic: statistic.into(),
        windows: windows.iter().map(|w| w.to_string()).collect(),
        rain_rate: Some(RainRateConfig {
            a: 200.0,
            b: 1.6,
            min_dbz: 7.0,
        }),
        threshold: (statistic == "count_above").then_some(1.0),
        min_coverage: 0.9,
        cadence: None,
        poll_interval_secs: 30,
        max_pixels: 4_000_000,
        max_reads_per_cycle: 16,
        read_strip_cells: 4 * 1024 * 1024,
    }
}

fn engine(source: &Arc<Source>, config: &PeriodConfig) -> PeriodEngine {
    PeriodEngine::new("rain", "radar", source.clone(), config).expect("valid config")
}

/// Poll until every window is built: a new window reads at most
/// `max_reads_per_cycle` frames a poll.
fn settle(engine: &PeriodEngine) {
    for _ in 0..4 {
        engine.poll_once();
    }
}

fn rate(dbz: f64) -> f64 {
    if dbz < 7.0 {
        0.0
    } else {
        (10f64.powf(dbz / 10.0) / 200.0).powf(1.0 / 1.6)
    }
}

/// The statistic over the window of `length` ending at the last of
/// `times`, computed directly in `f64` from the source's values.
fn direct(
    source: &Source,
    times: &[DateTime<Utc>],
    length: Duration,
    statistic: &str,
) -> Vec<Option<f64>> {
    let end = *times.last().unwrap();
    let frames: Vec<(DateTime<Utc>, f64)> = times
        .windows(2)
        .filter(|w| w[0] >= end - length)
        .map(|w| (w[0], ((w[1] - w[0]).num_seconds().min(300)) as f64 / 3600.0))
        .collect();
    (0..CELLS)
        .map(|cell| {
            let values: Vec<(f64, f64)> = frames
                .iter()
                .filter_map(|&(t, hours)| Some((rate(source.value(t, cell)?), hours)))
                .collect();
            if values.is_empty() {
                return None;
            }
            Some(match statistic {
                "sum" => values.iter().map(|(r, h)| r * h).sum(),
                "mean" => values.iter().map(|(r, _)| r).sum::<f64>() / values.len() as f64,
                _ => values.iter().filter(|(r, _)| *r >= 1.0).count() as f64,
            })
        })
        .collect()
}

/// The parameter's served grid, one value per cell (the CRS84 render at
/// the grid's own size samples each cell at its centre).
fn served(engine: &PeriodEngine, parameter: &str) -> Vec<Option<f64>> {
    let tile = engine
        .get_raster_tile(
            EXTENT,
            NX,
            NY,
            None,
            &OutputCrs::Wgs84,
            Some(parameter),
            None,
            None,
        )
        .unwrap();
    match tile.values {
        RasterValues::F32 { data, .. } => data
            .into_iter()
            .map(|v| v.is_finite().then_some(f64::from(v)))
            .collect(),
        _ => panic!("renders f32"),
    }
}

/// Largest difference relative to the field's largest value, asserting
/// both have values at the same cells.
fn max_relative(a: &[Option<f64>], b: &[Option<f64>]) -> f64 {
    let scale = b
        .iter()
        .flatten()
        .fold(0f64, |m, v| m.max(v.abs()))
        .max(1e-12);
    a.iter()
        .zip(b)
        .enumerate()
        .map(|(i, pair)| match pair {
            (Some(x), Some(y)) => (x - y).abs() / scale,
            (None, None) => 0.0,
            other => panic!("cell {i}: {other:?}"),
        })
        .fold(0.0, f64::max)
}

fn times(range: std::ops::Range<i64>) -> Vec<DateTime<Utc>> {
    range.map(step).collect()
}

/// Sum, mean and count_above over a fixture sequence match a direct
/// computation, from raw-byte and from `f32` frames.
#[test]
fn sum_mean_and_count_above_match_a_direct_computation() {
    for raw in [true, false] {
        let source = Source::new(times(0..20), raw);
        for statistic in ["sum", "mean", "count_above"] {
            let e = engine(&source, &config(statistic, &["PT30M", "PT1H"]));
            e.poll_once();
            let all = times(0..20);
            for (window, length) in [("PT30M", 30), ("PT1H", 60)] {
                let name = match statistic {
                    "sum" => format!("precipitation_amount_{window}"),
                    "mean" => format!("rain_rate_mean_{window}"),
                    _ => format!("rain_rate_count_above_{window}"),
                };
                let want = direct(&source, &all, Duration::minutes(length), statistic);
                let got = served(&e, &name);
                let err = max_relative(&got, &want);
                assert!(err < 1e-6, "{statistic} {window} raw={raw}: {err}");
                assert_eq!(e.parameter_times(&name).unwrap()[..], [step(19)]);
            }
        }
    }
}

/// Rolling updates over 300 steps equal a full recompute (a fresh engine
/// summing the same frames from scratch) to within 1e-6 of the field, and
/// the direct `f64` computation too. Every step reads the frame that just
/// got its successor and each window's leaving frame, 3 here, plus the
/// periodic re-sum's share: a window's frames once per four lengths.
#[test]
fn rolling_over_300_steps_equals_a_full_recompute() {
    let source = Source::new(times(0..40), false);
    let cfg = config("sum", &["PT1H", "PT3H"]);
    let rolling = engine(&source, &cfg);
    settle(&rolling);
    let mut worst = 0f64;
    let mut reads = Vec::new();
    for k in 40..340 {
        source.set_times(times(0..k + 1));
        let before = source.reads();
        rolling.poll_once();
        reads.push(source.reads() - before);
        if k % 25 == 0 || k == 339 {
            let fresh = engine(&source, &cfg);
            settle(&fresh);
            for window in ["PT1H", "PT3H"] {
                let name = format!("precipitation_amount_{window}");
                let full = served(&fresh, &name);
                let got = served(&rolling, &name);
                worst = worst.max(max_relative(&got, &full));
                let length = Duration::minutes(if window == "PT1H" { 60 } else { 180 });
                let want = direct(&source, &times(0..k + 1), length, "sum");
                assert!(max_relative(&got, &want) < 1e-6, "step {k} {window}");
            }
        }
    }
    assert!(worst < 1e-6, "rolling vs full recompute: {worst}");
    // Steady state: 1 new frame + 2 leaving frames, and the re-sum's
    // frames, never more than the budget.
    assert!(reads.iter().all(|&r| r <= 16), "{reads:?}");
    let mean = reads.iter().sum::<usize>() as f64 / reads.len() as f64;
    assert!(mean <= 4.0, "mean reads per step {mean}");
    assert_eq!(rolling.grid_bytes() % (6 * CELLS as u64), 0);
    // One strip of one frame at a time: here a strip is the whole frame.
    assert!(rolling.peak_frame_bytes() <= 4 * CELLS as u64);
    println!(
        "reads per step: mean {mean:.2}, max {}, worst relative {worst:.2e}, grid bytes {}, \
         peak frame bytes {}",
        reads.iter().max().unwrap(),
        rolling.grid_bytes(),
        rolling.peak_frame_bytes()
    );
}

/// The f32 running grids without any re-sum drift less than 1e-6 of the
/// field over 300 steps of one add and one remove each; the periodic
/// re-sum keeps it from building past that.
#[test]
fn f32_running_sums_drift_little_even_without_a_resum() {
    use engine_period::accum::{Frame, Grids, Op, Statistic};
    let source = Source::new(Vec::new(), false);
    let frame = |k: i64| {
        Frame::F32(
            (0..CELLS)
                .map(|c| {
                    source
                        .value(step(k), c)
                        .map_or(f32::NAN, |v| rate(v) as f32)
                })
                .collect(),
        )
    };
    let mut running = Grids::new(CELLS);
    for k in 0..12 {
        running.apply(&frame(k), Statistic::Sum, 300, Op::Add);
    }
    for k in 12..312 {
        running.apply(&frame(k), Statistic::Sum, 300, Op::Add);
        running.apply(&frame(k - 12), Statistic::Sum, 300, Op::Remove);
    }
    let mut scratch = Grids::new(CELLS);
    for k in 300..312 {
        scratch.apply(&frame(k), Statistic::Sum, 300, Op::Add);
    }
    let a: Vec<Option<f64>> = (0..CELLS)
        .map(|i| {
            Some(running.value(i, Statistic::Sum, true))
                .filter(|v| v.is_finite())
                .map(f64::from)
        })
        .collect();
    let b: Vec<Option<f64>> = (0..CELLS)
        .map(|i| {
            Some(scratch.value(i, Statistic::Sum, true))
                .filter(|v| v.is_finite())
                .map(f64::from)
        })
        .collect();
    let err = max_relative(&a, &b);
    println!("f32 drift over 300 steps without a re-sum: {err:.2e}");
    assert!(err < 1e-5, "{err}");
}

/// Below `min_coverage` nothing is served for that time; above it, the
/// coverage parameter shows the share.
#[test]
fn coverage_gates_what_is_served() {
    // Two frames missing from the last hour: 10 of 12 → 0.833 < 0.9.
    let mut t = times(0..30);
    t.retain(|&x| x != step(22) && x != step(25));
    let source = Source::new(t, false);
    let e = engine(&source, &config("sum", &["PT1H", "PT2H"]));
    settle(&e);
    assert!(e
        .parameter_times("precipitation_amount_PT1H")
        .unwrap()
        .is_empty());
    assert!(e.parameter_times("coverage_PT1H").unwrap().is_empty());
    assert!(served(&e, "precipitation_amount_PT1H")
        .iter()
        .all(Option::is_none));
    assert_eq!(
        e.resolve_parameter_time(Some("coverage_PT1H"), None, None),
        None
    );
    let err = e
        .query_position(
            "POINT(10.5 60.5)",
            None,
            Some(&["precipitation_amount_PT1H".into()]),
            None,
            None,
        )
        .unwrap_err();
    assert!(
        matches!(err, DataServerError::LocationNotFound(_)),
        "{err:?}"
    );
    assert!(e.waiting_for().unwrap().contains("PT1H: coverage 0.833"));

    // PT2H: 22 of 24 frames → 0.9167, served; its coverage parameter shows
    // it where every frame has a value, less where some do not.
    assert_eq!(e.parameter_times("coverage_PT2H").unwrap()[..], [step(29)]);
    let coverage = served(&e, "coverage_PT2H");
    let full = 22.0 / 24.0;
    let complete = (0..CELLS)
        .filter(|&c| !c.is_multiple_of(17) && times(5..29).iter().all(|&t| dbz(t, c).is_some()))
        .collect::<Vec<_>>();
    assert!(!complete.is_empty());
    for c in complete {
        assert!(
            (coverage[c].unwrap() - full).abs() < 1e-6,
            "{:?}",
            coverage[c]
        );
    }
    assert!(coverage[0].is_none(), "nodata in every frame stays nodata");
    let partial = coverage.iter().flatten().fold(1f64, |m, &v| m.min(v));
    assert!(partial < full - 0.01);
    assert!(e.has_data());

    // The gap filled late: PT1H reaches 12 of 12 and is served.
    source.set_times(times(0..30));
    e.poll_once();
    assert_eq!(e.parameter_times("coverage_PT1H").unwrap()[..], [step(29)]);
    let want = direct(&source, &times(0..30), Duration::hours(1), "sum");
    assert!(max_relative(&served(&e, "precipitation_amount_PT1H"), &want) < 1e-6);
}

/// A late frame inside a served window changes what it serves at the same
/// end, and so its version; a window it is not in keeps its version.
#[test]
fn a_late_frame_moves_the_content_version() {
    let mut t = times(0..40);
    t.retain(|&x| x != step(37));
    let source = Source::new(t, false);
    let e = engine(&source, &config("sum", &["PT20M", "PT2H"]));
    settle(&e);
    let end = step(39);
    let long = "precipitation_amount_PT2H";
    let short = "precipitation_amount_PT20M";
    assert_eq!(e.resolve_parameter_time(Some(long), None, None), Some(end));
    let (v_long, v_short, v_all) = (
        e.content_version_for(Some(long), Some(end), None, None),
        e.content_version_for(Some(short), Some(end), None, None),
        e.content_version(),
    );
    assert!(v_long > 0 && v_short > 0);
    let before = served(&e, long);

    // A poll that changes nothing moves nothing.
    e.poll_once();
    assert_eq!(
        e.content_version_for(Some(long), Some(end), None, None),
        v_long
    );
    assert_eq!(e.content_version(), v_all);

    // Frame 37 arrives late: inside both windows ([35, 39) and [15, 39)).
    source.set_times(times(0..40));
    e.poll_once();
    assert_eq!(e.resolve_parameter_time(Some(long), None, None), Some(end));
    assert_ne!(
        e.content_version_for(Some(long), Some(end), None, None),
        v_long
    );
    assert_ne!(
        e.content_version_for(Some(short), Some(end), None, None),
        v_short
    );
    assert!(e.content_version() > v_all);
    assert_ne!(served(&e, long), before);

    // A late frame only the long window holds leaves the short one's
    // version alone.
    let mut t = times(0..41);
    t.retain(|&x| x != step(30));
    source.set_times(t);
    e.poll_once();
    let v_short = e.content_version_for(Some(short), None, None, None);
    let v_long = e.content_version_for(Some(long), None, None, None);
    source.set_times(times(0..41));
    e.poll_once();
    assert_eq!(
        e.content_version_for(Some(short), None, None, None),
        v_short
    );
    assert_ne!(e.content_version_for(Some(long), None, None, None), v_long);
}

/// A window is built once the source holds its history, and health says
/// why until then.
#[test]
fn a_source_holding_too_little_history_is_reported() {
    let source = Source::new(times(0..12), false);
    let e = engine(&source, &config("sum", &["PT1H"]));
    e.poll_once();
    assert!(!e.has_data());
    let why = e.waiting_for().unwrap();
    assert!(
        why.contains("the source holds 55 min of frames; this window needs 1 h"),
        "{why}"
    );
    source.set_times(times(0..13));
    e.poll_once();
    assert!(e.has_data());
}

/// A source that keeps exactly one window of frames drops each frame while
/// it is still in the window. The window breaks once, then waits for a
/// source that holds one frame more, saying so, instead of rebuilding
/// every poll.
#[test]
fn a_source_dropping_frames_inside_the_window_is_reported() {
    let source = Source::new(times(0..13), false);
    let e = engine(&source, &config("sum", &["PT1H"]));
    e.poll_once();
    assert!(e.has_data());
    let mut reads = 0;
    for k in 13..20 {
        source.set_times(times(k - 12..k + 1));
        let before = source.reads();
        e.poll_once();
        reads += source.reads() - before;
        assert!(!e.has_data(), "step {k}");
    }
    assert!(
        reads <= 3,
        "no rebuild while the source holds too little: {reads}"
    );
    let why = e.waiting_for().unwrap();
    assert!(
        why.contains("needs 1 h 5 min (the window and one frame before it"),
        "{why}"
    );
    source.set_times(times(5..20));
    e.poll_once();
    assert!(e.has_data());
}

/// A frame that cannot be read is left out, its window's coverage shows
/// the gap, and it is read again later.
#[test]
fn an_unreadable_frame_lowers_coverage_until_it_reads() {
    let source = Source::new(times(0..30), false);
    source.broken.lock().unwrap().push((step(26), 0));
    let mut cfg = config("sum", &["PT1H"]);
    cfg.min_coverage = 0.5;
    let e = engine(&source, &cfg);
    e.poll_once();
    e.poll_once();
    let built = served(&e, "coverage_PT1H");
    let c = (0..CELLS)
        .find(|&c| !c.is_multiple_of(17) && times(17..29).iter().all(|&t| dbz(t, c).is_some()))
        .unwrap();
    assert!(
        (built[c].unwrap() - 11.0 / 12.0).abs() < 1e-6,
        "{:?}",
        built[c]
    );
    source.broken.lock().unwrap().clear();
    for _ in 0..25 {
        e.poll_once();
    }
    assert!((served(&e, "coverage_PT1H")[c].unwrap() - 1.0).abs() < 1e-6);
    let want = direct(&source, &times(0..30), Duration::hours(1), "sum");
    assert!(max_relative(&served(&e, "precipitation_amount_PT1H"), &want) < 1e-6);
}

/// EDR position and area read the same grid the map draws.
#[test]
fn edr_position_and_area_answer_the_map_values() {
    let source = Source::new(times(0..20), true);
    let e = engine(&source, &config("sum", &["PT1H"]));
    e.poll_once();
    let map = served(&e, "precipitation_amount_PT1H");
    let cell = (NORTH - 62.5).floor() as usize * NX as usize + 5;
    let CoverageResponse::Single(point) = e
        .query_position("POINT(5.5 62.5)", None, None, None, None)
        .unwrap()
    else {
        panic!("one coverage")
    };
    let values = &point.ranges["precipitation_amount_PT1H"].values;
    assert_eq!(values.len(), 1);
    assert!((values[0].unwrap() - map[cell].unwrap()).abs() < 1e-6);
    assert!(point.ranges.contains_key("coverage_PT1H"));

    let CoverageResponse::Single(area) = e
        .query_area(
            "POLYGON((4 60, 8 60, 8 63, 4 63, 4 60))",
            Some((step(19), step(19))),
            None,
            None,
            None,
        )
        .unwrap()
    else {
        panic!("one coverage")
    };
    let DomainDescription::Grid { x, y, t, .. } = &area.domain else {
        panic!("a grid")
    };
    assert_eq!(t.as_deref(), Some(&[step(19)][..]));
    let range = &area.ranges["precipitation_amount_PT1H"];
    assert_eq!(range.shape, vec![1, y.len(), x.len()]);
    for (iy, &lat) in y.iter().enumerate() {
        for (ix, &lon) in x.iter().enumerate() {
            let cell = ((NORTH - lat).floor() as usize) * NX as usize + lon.floor() as usize;
            let got = range.values[iy * x.len() + ix];
            match (got, map[cell]) {
                (Some(g), Some(m)) => assert!((g - m).abs() < 1e-6),
                (None, None) => {}
                other => panic!("({lon}, {lat}): {other:?}"),
            }
        }
    }
    // A datetime the window does not end in serves nothing.
    assert!(e
        .query_position(
            "POINT(5.5 62.5)",
            Some((step(10), step(12))),
            None,
            None,
            None
        )
        .is_err());
}

/// Bad configs fail at construction: an unknown parameter, a bad window,
/// a statistic the source cannot carry, max/min.
#[test]
fn bad_configs_fail_the_collection() {
    let source = Source::new(times(0..20), false);
    let fails = |c: PeriodConfig| {
        PeriodEngine::new("rain", "radar", source.clone(), &c)
            .err()
            .unwrap()
            .to_string()
    };
    let mut c = config("sum", &["PT1H"]);
    c.parameter = Some("rain".into());
    assert!(fails(c).contains("no parameter 'rain'"));
    for windows in [vec!["1 hour"], vec!["PT0S"], vec![], vec!["PT1H", "PT60M"]] {
        assert!(!fails(config("sum", &windows)).is_empty());
    }
    let mut c = config("sum", &["PT1H"]);
    c.rain_rate = None;
    assert!(fails(c).contains("not a rate per hour"));
    assert!(fails(config("max", &["PT1H"])).contains("not supported yet"));
    let mut c = config("count_above", &["PT1H"]);
    c.threshold = None;
    assert!(fails(c).contains("threshold"));
    let mut c = config("sum", &["PT1H", "PT3H"]);
    c.max_reads_per_cycle = 2;
    assert!(fails(c).contains("max_reads_per_cycle"));
    let mut c = config("mean", &["PT1H"]);
    c.min_coverage = 0.0;
    assert!(fails(c).contains("min_coverage"));
}

/// The windows' grids are 6 bytes a cell each, whatever their length, and
/// a re-sum adds one window's at most.
#[test]
fn memory_is_six_bytes_a_cell_per_window() {
    let source = Source::new(times(0..400), false);
    let e = engine(&source, &config("sum", &["PT1H", "PT3H", "PT24H"]));
    for _ in 0..20 {
        e.poll_once();
    }
    assert!(e.has_data());
    let per_window = 6 * CELLS as u64;
    for k in 400..700 {
        source.set_times(times(0..k));
        e.poll_once();
        let bytes = e.grid_bytes();
        assert!(
            bytes == 3 * per_window || bytes == 4 * per_window,
            "{bytes}"
        );
    }
}

/// Folding frames in strips of four rows (4 + 4 + 4 + 4 + 1 over the odd
/// 17 rows) leaves the grids bit for bit as folding them whole, over
/// steps that add, take out and re-sum, from raw-byte and `f32` frames:
/// strips line up with the grid's rows, none read twice or skipped.
#[test]
fn strips_fold_bit_for_bit_like_whole_frames() {
    for raw in [false, true] {
        let source = Source::new(times(0..40), raw);
        let mut whole_cfg = config("sum", &["PT1H", "PT3H"]);
        whole_cfg.read_strip_cells = CELLS;
        let mut strip_cfg = whole_cfg.clone();
        strip_cfg.read_strip_cells = 4 * NX as usize;
        let whole = engine(&source, &whole_cfg);
        let strips = engine(&source, &strip_cfg);
        settle(&whole);
        settle(&strips);
        let names = [
            "precipitation_amount_PT1H",
            "precipitation_amount_PT3H",
            "coverage_PT1H",
            "coverage_PT3H",
        ];
        for k in 40..160 {
            source.set_times(times(0..k + 1));
            whole.poll_once();
            let (reads, strip_reads) = (source.reads(), source.strip_reads.load(Ordering::SeqCst));
            strips.poll_once();
            let frames = source.reads() - reads;
            assert_eq!(
                source.strip_reads.load(Ordering::SeqCst) - strip_reads,
                5 * frames
            );
            for name in names {
                let bits = |e: &PeriodEngine| -> Vec<u32> {
                    served(e, name)
                        .iter()
                        .map(|v| v.map_or(u32::MAX, |v| (v as f32).to_bits()))
                        .collect()
                };
                assert_eq!(bits(&whole), bits(&strips), "step {k} {name} raw={raw}");
            }
            assert_eq!(
                whole.content_version_for(Some(names[0]), None, None, None),
                strips.content_version_for(Some(names[0]), None, None, None)
            );
        }
        // Every frame read had each of its rows read exactly as often.
        for (t, counts) in source.rows_read.lock().unwrap().iter() {
            assert!(
                counts.iter().all(|&c| c == counts[0] && c > 0),
                "{t}: {counts:?}"
            );
        }
        // A strip of four rows of f32 is the most a poll holds.
        if !raw {
            assert!(strips.peak_frame_bytes() <= 4 * NX as u64 * 4);
            assert!(whole.peak_frame_bytes() >= CELLS as u64 * 4);
        }
    }
}

/// A frame whose read fails part way through its strips breaks the
/// windows it reached (part of a frame cannot be taken out again), which
/// are then rebuilt to the right sums once it reads.
#[test]
fn a_frame_failing_part_way_breaks_and_rebuilds() {
    let source = Source::new(times(0..30), false);
    let mut cfg = config("sum", &["PT1H"]);
    cfg.read_strip_cells = 4 * NX as usize;
    let e = engine(&source, &cfg);
    settle(&e);
    assert!(e.has_data());
    // Frame 29 gets its successor at step 30; its third strip fails.
    source.broken.lock().unwrap().push((step(29), 8));
    source.set_times(times(0..31));
    e.poll_once();
    assert!(!e.has_data(), "the window took part of frame 29: broken");
    assert!(e.waiting_for().unwrap().contains("could not be read whole"));
    source.broken.lock().unwrap().clear();
    for _ in 0..25 {
        e.poll_once();
    }
    let want = direct(&source, &times(0..31), Duration::hours(1), "sum");
    assert!(max_relative(&served(&e, "precipitation_amount_PT1H"), &want) < 1e-6);
}

/// A map cache key as the API layers build one: the resolved time and the
/// content version of the parameter's window.
type CacheKey = (Option<DateTime<Utc>>, u64);

fn cache_key(e: &PeriodEngine, parameter: &str) -> CacheKey {
    (
        e.resolve_parameter_time(Some(parameter), None, None),
        e.content_version_for(Some(parameter), None, None, None),
    )
}

/// A render taken while a poll folds a late frame into a window, between
/// its strips, is keyed on an in-flux version: never the window's pre-fold
/// key, whose cached image stays the consistent one, and never the key
/// served once the poll publishes, which moves again. The end does not
/// move (a late frame), so only the version can tell them apart.
#[test]
fn a_render_mid_fold_never_lands_under_a_live_key() {
    let mut late = times(0..40);
    late.retain(|&x| x != step(37));
    let source = Source::new(late, false);
    let mut cfg = config("sum", &["PT1H"]);
    cfg.read_strip_cells = 4 * NX as usize;
    let e = Arc::new(engine(&source, &cfg));
    settle(&e);
    let p = "precipitation_amount_PT1H";
    // The API layers' no-TTL map cache: key → image.
    let mut cache: HashMap<CacheKey, Vec<Option<f64>>> = HashMap::new();
    let before = cache_key(&e, p);
    let pre_image = served(&e, p);
    cache.insert(before, pre_image.clone());

    // Frame 37 arrives late. Render while its strips are read: after one,
    // two and three of its five strips went in.
    type Seen = Vec<(u32, CacheKey, Vec<Option<f64>>)>;
    let seen: Arc<Mutex<Seen>> = Arc::default();
    {
        let (engine, seen) = (e.clone(), seen.clone());
        *source.hook.lock().unwrap() = Some(Box::new(move |t, row| {
            if t == step(37) && row > 0 {
                let image = served(&engine, p);
                seen.lock()
                    .unwrap()
                    .push((row, cache_key(&engine, p), image));
            }
        }));
    }
    source.set_times(times(0..40));
    e.poll_once();
    *source.hook.lock().unwrap() = None;

    let seen = std::mem::take(&mut *seen.lock().unwrap());
    assert_eq!(
        seen.iter().map(|s| s.0).collect::<Vec<_>>(),
        vec![4, 8, 12, 16]
    );
    let flux = seen[0].1;
    for (row, key, image) in &seen {
        assert_eq!(
            *key, flux,
            "one in-flux version for the whole fold (row {row})"
        );
        assert_eq!(key.0, before.0, "a late frame keeps the end");
        assert_ne!(key.1, before.1, "never the pre-fold version (row {row})");
        // A cache miss mid-fold fills the cache under its own key.
        cache.insert(*key, image.clone());
    }
    assert_eq!(
        cache[&before], pre_image,
        "the pre-fold entry keeps the consistent image"
    );
    assert_ne!(seen[1].2, pre_image, "part of frame 37 went in by row 8");

    let after = cache_key(&e, p);
    assert_eq!(after.0, before.0);
    assert!(
        after.1 != flux.1 && after.1 != before.1,
        "{after:?} {flux:?} {before:?}"
    );
    assert!(
        !cache.contains_key(&after),
        "the published key misses and renders afresh"
    );
    let image = served(&e, p);
    for (row, _, partial) in &seen {
        assert_ne!(
            *partial, image,
            "row {row}: the mid-fold image is not the final one"
        );
    }
    let want = direct(&source, &times(0..40), Duration::hours(1), "sum");
    assert!(max_relative(&image, &want) < 1e-6);
    // A poll that changes nothing moves nothing.
    e.poll_once();
    assert_eq!(cache_key(&e, p), after);
}

/// One transient read failure breaks at most the window that had to take
/// the frame out: a longer window that holds the frame keeps it, and keeps
/// serving. (A failed frame used to drop out of every target, so every
/// window holding it had to take it out and broke for ~20 polls.)
#[test]
fn one_failed_read_breaks_no_longer_window() {
    let source = Source::new(times(0..300), false);
    let e = engine(&source, &config("sum", &["PT1H", "PT24H"]));
    for _ in 0..25 {
        e.poll_once();
    }
    assert_eq!(
        e.parameter_times("precipitation_amount_PT24H").unwrap()[..],
        [step(299)]
    );
    // PT1H's next leaving frame, 287, fails for three polls (an S3 error,
    // the GeoTIFF decode budget's ResourceExhausted under load).
    source.broken.lock().unwrap().push((step(287), 0));
    for k in 300..303 {
        source.set_times(times(0..k + 1));
        e.poll_once();
        assert_eq!(
            e.parameter_times("precipitation_amount_PT24H").unwrap()[..],
            [step(k)],
            "PT24H holds frame 287 and keeps serving"
        );
        if k == 300 {
            // PT1H had to take 287 out: it alone broke, and rebuilds
            // without it at the next poll.
            assert!(e
                .parameter_times("precipitation_amount_PT1H")
                .unwrap()
                .is_empty());
            assert!(e.waiting_for().unwrap().contains("PT1H: rebuilding"));
        }
    }
    source.broken.lock().unwrap().clear();
    for k in 303..305 {
        source.set_times(times(0..k + 1));
        e.poll_once();
        assert_eq!(
            e.parameter_times("precipitation_amount_PT24H").unwrap()[..],
            [step(k)]
        );
    }
    let all = times(0..305);
    for (name, length) in [("PT1H", 1), ("PT24H", 24)] {
        let want = direct(&source, &all, Duration::hours(length), "sum");
        let got = served(&e, &format!("precipitation_amount_{name}"));
        assert!(max_relative(&got, &want) < 1e-6, "{name}");
    }
}

/// A new frame that fails once is read again at the next poll, not ten
/// minutes later: a 20-minute window of 5-minute frames is back at full
/// coverage one poll after the failure.
#[test]
fn a_failed_new_frame_is_read_again_at_the_next_poll() {
    let source = Source::new(times(0..20), false);
    let e = engine(&source, &config("sum", &["PT20M"]));
    settle(&e);
    // Frame 19 gets its successor and fails once.
    source.broken.lock().unwrap().push((step(19), 0));
    source.set_times(times(0..21));
    e.poll_once();
    assert!(
        e.parameter_times("coverage_PT20M").unwrap().is_empty(),
        "15 of 20 min"
    );
    source.broken.lock().unwrap().clear();
    e.poll_once();
    assert_eq!(e.parameter_times("coverage_PT20M").unwrap()[..], [step(20)]);
    let want = direct(&source, &times(0..21), Duration::minutes(20), "sum");
    assert!(max_relative(&served(&e, "precipitation_amount_PT20M"), &want) < 1e-6);
}

/// When every window breaks at once (a burst after a source outage), each
/// window's grids go before its rebuild's are allocated: the poll never
/// holds more than the windows' grids and one more, plus a strip.
#[test]
fn breaking_every_window_holds_no_more_than_one_extra_grid() {
    let source = Source::new(times(0..100), false);
    let mut cfg = config("sum", &["PT1H", "PT3H", "PT6H"]);
    cfg.read_strip_cells = 4 * NX as usize;
    let e = engine(&source, &cfg);
    for _ in 0..8 {
        e.poll_once();
    }
    let grid = 6 * CELLS as u64;
    assert_eq!(e.grid_bytes(), 3 * grid);
    // 20 frames at once: more than max_reads_per_cycle for every window.
    source.set_times(times(0..120));
    e.poll_once();
    // Every window broke; PT1H's rebuild already finished in the same poll.
    let why = e.waiting_for().unwrap();
    assert!(
        why.contains("PT3H: rebuilding") && why.contains("PT6H: rebuilding"),
        "{why}"
    );
    assert!(
        e.peak_grid_bytes() <= 4 * grid,
        "{} > {}",
        e.peak_grid_bytes(),
        4 * grid
    );
    assert!(e.peak_frame_bytes() <= 4 * NX as u64 * 4);
    for _ in 0..8 {
        e.poll_once();
    }
    assert!(e.has_data());
    assert!(e.peak_grid_bytes() <= 4 * grid);
}

/// A frame the source revised in place since a window added it is never
/// subtracted with its new values: the strip reads back with another
/// digest, the window breaks and is rebuilt from the revised frames. The
/// revision gives cells that were nodata a value, so subtracting it would
/// take a count below zero (the `debug_assert!` in `accum::step`).
#[test]
fn a_frame_revised_in_place_is_never_subtracted_with_its_new_values() {
    let source = Source::new(times(0..30), false);
    let mut cfg = config("sum", &["PT1H"]);
    cfg.read_strip_cells = 4 * NX as usize;
    let e = engine(&source, &cfg);
    settle(&e);
    // Frame 17, the next to leave, is revised in place; the source's
    // version does not move (a source may not say).
    source.revised.lock().unwrap().insert(step(17));
    source.set_times(times(0..31));
    e.poll_once();
    assert!(e.waiting_for().unwrap().contains("changed in the source"));
    for _ in 0..3 {
        e.poll_once();
    }
    let want = direct(&source, &times(0..31), Duration::hours(1), "sum");
    assert!(max_relative(&served(&e, "precipitation_amount_PT1H"), &want) < 1e-6);
}

/// A window shorter than the source's frame spacing never holds a frame:
/// refused at load with a configured cadence, explained in health without.
#[test]
fn a_window_shorter_than_the_cadence_is_refused_or_explained() {
    let source = Source::new(times(0..20), false);
    let mut cfg = config("sum", &["PT4M", "PT1H"]);
    cfg.cadence = Some("PT5M".into());
    let err = PeriodEngine::new("rain", "radar", source.clone(), &cfg)
        .err()
        .unwrap()
        .to_string();
    assert!(
        err.contains("window 'PT4M' is shorter than the cadence"),
        "{err}"
    );
    cfg.cadence = None;
    let e = engine(&source, &cfg);
    settle(&e);
    let why = e.waiting_for().unwrap();
    assert!(
        why.contains("PT4M: the window (4 min) is shorter than the source's frame spacing (5 min)"),
        "{why}"
    );
}

/// A source that loses its grid serves nothing: every window is dropped
/// (health stops saying ready), and rebuilt once the grid is back.
#[test]
fn a_source_losing_its_grid_serves_nothing_until_it_is_back() {
    let source = Source::new(times(0..20), false);
    let e = engine(&source, &config("sum", &["PT1H"]));
    settle(&e);
    assert!(e.has_data());
    let before = e.content_version_for(Some("precipitation_amount_PT1H"), None, None, None);
    source.lost_grid.store(true, Ordering::SeqCst);
    e.poll_once();
    assert!(!e.has_data());
    assert!(e
        .parameter_times("precipitation_amount_PT1H")
        .unwrap()
        .is_empty());
    assert!(e
        .waiting_for()
        .unwrap()
        .contains("the source reports no grid"));
    assert_ne!(
        e.content_version_for(Some("precipitation_amount_PT1H"), None, None, None),
        before
    );
    assert_eq!(e.grid_bytes(), 0);
    source.lost_grid.store(false, Ordering::SeqCst);
    settle(&e);
    assert!(e.has_data());
}

/// A frame that keeps failing while a window holds it stays in that
/// window: the live window keeps serving at full coverage, and a re-sum
/// running meanwhile waits for the frame instead of swapping in grids
/// without it, then finishes once it reads.
#[test]
fn a_held_frame_that_keeps_failing_stays_held() {
    let source = Source::new(times(0..30), false);
    let e = engine(&source, &config("sum", &["PT1H"]));
    settle(&e);
    let c = (0..CELLS)
        .find(|&c| !c.is_multiple_of(17) && times(17..29).iter().all(|&t| dbz(t, c).is_some()))
        .unwrap();
    // The source revises (a re-sum starts) while held frame 20 fails.
    source.broken.lock().unwrap().push((step(20), 0));
    source.version.fetch_add(1, Ordering::SeqCst);
    for poll in 0..6 {
        e.poll_once();
        assert!(e.has_data(), "poll {poll}");
        let coverage = served(&e, "coverage_PT1H")[c].unwrap();
        assert!((coverage - 1.0).abs() < 1e-6, "poll {poll}: {coverage}");
    }
    source.broken.lock().unwrap().clear();
    for _ in 0..10 {
        e.poll_once();
    }
    let want = direct(&source, &times(0..30), Duration::hours(1), "sum");
    assert!(max_relative(&served(&e, "precipitation_amount_PT1H"), &want) < 1e-6);
    assert!((served(&e, "coverage_PT1H")[c].unwrap() - 1.0).abs() < 1e-6);
}
