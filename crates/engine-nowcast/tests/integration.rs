//! End-to-end engine tests against a synthetic translating-echo source.
//!
//! The mock source is the truth generator: a hard disc of reflectivity
//! translating at a constant pixel velocity, rendered for ANY requested time
//! from a closed-form position. That lets the tests verify the engine's
//! extrapolation against the source's actual future — the phase-0 gate wired
//! through the full `MapEngine` surface.

use std::sync::{Arc, RwLock};

use chrono::{DateTime, Duration, Utc};

use ds_core::config::NowcastConfig;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile, RasterValues};
use engine_nowcast::NowcastEngine;

const W: u32 = 200;
const H: u32 = 200;
/// WGS84 extent of the mock grid: 10°×10°.
const EXTENT: [f64; 4] = [0.0, 50.0, 10.0, 60.0];
/// Disc translation in pixels per 5-minute frame interval.
const DX_PER_FRAME: f64 = 2.0;
/// Raw byte for ~40 dBZ under gain 0.4 / offset −30.
const ECHO_RAW: u8 = 175;
const NODATA: u8 = 255;

fn t0() -> DateTime<Utc> {
    "2026-07-20T12:00:00Z".parse().unwrap()
}

/// Disc centre (px) at time `t`: starts at (60, 100), moves +x only.
fn disc_center(t: DateTime<Utc>) -> (f64, f64) {
    let intervals = (t - t0()).num_seconds() as f64 / 300.0;
    (60.0 + DX_PER_FRAME * intervals, 100.0)
}

/// Render the truth frame for time `t` as raw bytes (0 = clear, 175 = echo).
fn truth_frame(t: DateTime<Utc>) -> Vec<u8> {
    let (cx, cy) = disc_center(t);
    let mut data = vec![0u8; (W * H) as usize];
    for (i, cell) in data.iter_mut().enumerate() {
        let x = (i % W as usize) as f64 + 0.5;
        let y = (i / W as usize) as f64 + 0.5;
        if (x - cx).powi(2) + (y - cy).powi(2) <= 15.0 * 15.0 {
            *cell = ECHO_RAW;
        }
    }
    data
}

/// Mock raster source: advertises a mutable frame catalog and renders the
/// closed-form disc for whatever time the engine asks.
struct MockSource {
    times: RwLock<Vec<DateTime<Utc>>>,
}

impl MapEngine for MockSource {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        _output_crs: &OutputCrs,
        _parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        assert_eq!((width, height), (W, H), "engine must fetch the native grid");
        let t = time.ok_or_else(|| DataServerError::Engine("mock needs a time".into()))?;
        Ok(RasterTile {
            width,
            height,
            values: RasterValues::U8 {
                data: truth_frame(t),
                nodata: Some(NODATA),
                gain: 0.4,
                offset: -30.0,
            },
        })
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: Some(EXTENT),
            times: self.times.read().unwrap().clone(),
            parameter: "reflectivity".into(),
            unit: "dBZ".into(),
            parameters: vec![],
            vertical: None,
            grid_size: Some([W, H]),
            layer_subtitle: None,
            reference_times: Vec::new(),
        }
    }
}

fn build(horizon: &str, source_times: &[DateTime<Utc>]) -> (Arc<MockSource>, NowcastEngine) {
    build_with_history(horizon, source_times, 2)
}

fn build_with_history(
    horizon: &str,
    source_times: &[DateTime<Utc>],
    history_frames: usize,
) -> (Arc<MockSource>, NowcastEngine) {
    let source = Arc::new(MockSource {
        times: RwLock::new(source_times.to_vec()),
    });
    let config = NowcastConfig {
        source: "mock".into(),
        horizon: horizon.into(),
        step: None,
        history_frames,
        poll_interval_secs: 30,
        max_generations: 4,
        max_pixels: 4_000_000,
        min_echo: 10.0,
    };
    let engine =
        NowcastEngine::new("mock-nowcast", "mock", source.clone(), &config).expect("engine builds");
    (source, engine)
}

/// Multi-pair motion (#524): a three-frame history must extrapolate the
/// translating source just as accurately end to end.
#[test]
fn multi_pair_history_tracks_the_source() {
    let anchor = t0() + Duration::minutes(10);
    let (_source, engine) =
        build_with_history("PT1H", &[t0(), t0() + Duration::minutes(5), anchor], 3);
    engine.poll_once();
    assert!(engine.has_data());

    let lead_time = anchor + Duration::minutes(30);
    let forecast = render_raw(&engine, lead_time);
    let truth = truth_frame(lead_time);
    let (mut inter, mut union) = (0u32, 0u32);
    for (f, o) in forecast.iter().zip(&truth) {
        let (fe, oe) = (*f == ECHO_RAW, *o == ECHO_RAW);
        if fe && oe {
            inter += 1;
        }
        if fe || oe {
            union += 1;
        }
    }
    let iou = inter as f64 / union as f64;
    assert!(iou > 0.8, "multi-pair extrapolation must track (IoU {iou})");
}

/// Raw bytes of a full-extent render at `time`.
fn render_raw(engine: &NowcastEngine, time: DateTime<Utc>) -> Vec<u8> {
    let tile = engine
        .get_raster_tile(
            EXTENT,
            W,
            H,
            Some(time),
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
        .expect("render succeeds");
    match tile.values {
        RasterValues::U8 { data, nodata, .. } => {
            assert_eq!(nodata, Some(NODATA), "nodata byte must survive end to end");
            data
        }
        RasterValues::F64(_) => panic!("U8 source must stay on the raw-byte path"),
    }
}

#[test]
fn generation_produces_future_frames_and_instances_contract() {
    let anchor = t0() + Duration::minutes(5);
    let (_source, engine) = build("PT1H", &[t0(), anchor]);
    assert!(!engine.has_data(), "no generation before the first poll");

    engine.poll_once();
    assert!(engine.has_data());

    let info = engine.raster_info();
    // Analysis frame + 12 five-minute leads over PT1H.
    assert_eq!(info.times.len(), 13);
    assert_eq!(info.times[0], anchor);
    assert_eq!(*info.times.last().unwrap(), anchor + Duration::hours(1));
    assert_eq!(info.reference_times, vec![anchor]);
    assert_eq!(info.grid_size, Some([W, H]));

    // #521 contract: None resolves to the concrete latest generation.
    assert_eq!(engine.resolve_reference_time(None, None), Some(anchor));
    // #507 contract: latest-not-after snapping onto the generated steps.
    assert_eq!(
        engine.resolve_time(Some(anchor + Duration::minutes(17)), None),
        Some(anchor + Duration::minutes(15))
    );
}

#[test]
fn extrapolation_tracks_the_translating_source() {
    let anchor = t0() + Duration::minutes(5);
    let (_source, engine) = build("PT1H", &[t0(), anchor]);
    engine.poll_once();

    // +30 minutes: compare the engine's extrapolation against the source's
    // actual (closed-form) future frame. Pure translation should recover
    // nearly the exact disc; require IoU well above what a stationary
    // (persistence) disc could score.
    let lead_time = anchor + Duration::minutes(30);
    let forecast = render_raw(&engine, lead_time);
    let truth = truth_frame(lead_time);
    let (mut inter, mut union) = (0u32, 0u32);
    for (f, o) in forecast.iter().zip(&truth) {
        let (fe, oe) = (*f == ECHO_RAW, *o == ECHO_RAW);
        if fe && oe {
            inter += 1;
        }
        if fe || oe {
            union += 1;
        }
    }
    let iou = inter as f64 / union as f64;
    assert!(
        iou > 0.8,
        "extrapolated disc must track the truth (IoU {iou})"
    );

    // Persistence baseline for context: the disc moved 12 px over 6
    // intervals; a stationary 15 px-radius disc overlaps ~55% — the gate
    // ensures we beat that by a wide margin.
    let persistence = truth_frame(anchor);
    let (mut p_inter, mut p_union) = (0u32, 0u32);
    for (f, o) in persistence.iter().zip(&truth) {
        let (fe, oe) = (*f == ECHO_RAW, *o == ECHO_RAW);
        if fe && oe {
            p_inter += 1;
        }
        if fe || oe {
            p_union += 1;
        }
    }
    let p_iou = p_inter as f64 / p_union as f64;
    assert!(
        iou > p_iou,
        "extrapolation (IoU {iou}) must beat persistence (IoU {p_iou})"
    );
}

/// The #507 contract wired end to end: rendering at a raw between-steps
/// instant produces byte-identical output to rendering at the resolved step.
#[test]
fn resolve_time_and_render_share_one_selection() {
    let anchor = t0() + Duration::minutes(5);
    let (_source, engine) = build("PT1H", &[t0(), anchor]);
    engine.poll_once();

    let raw = anchor + Duration::minutes(23);
    let resolved = engine.resolve_time(Some(raw), None).unwrap();
    assert_eq!(resolved, anchor + Duration::minutes(20));
    assert_eq!(
        render_raw(&engine, raw),
        render_raw(&engine, resolved),
        "raw-instant render must serve the resolved step's frame"
    );
}

#[test]
fn new_source_frame_rolls_a_new_generation() {
    let anchor1 = t0() + Duration::minutes(5);
    let (source, engine) = build("PT30M", &[t0(), anchor1]);
    engine.poll_once();
    assert_eq!(engine.raster_info().reference_times, vec![anchor1]);

    // Re-polling without new source data must not add generations.
    engine.poll_once();
    assert_eq!(engine.raster_info().reference_times.len(), 1);

    // A new source frame lands → new generation, ascending reference_times,
    // and the run axis resolves to the new one (#521).
    let anchor2 = anchor1 + Duration::minutes(5);
    source.times.write().unwrap().push(anchor2);
    engine.poll_once();
    let info = engine.raster_info();
    assert_eq!(info.reference_times, vec![anchor1, anchor2]);
    assert_eq!(engine.resolve_reference_time(None, None), Some(anchor2));
    // Pinning the previous generation still serves it.
    assert_eq!(
        engine.resolve_reference_time(None, Some(anchor1)),
        Some(anchor1)
    );
    assert_eq!(
        engine.resolve_time(None, Some(anchor1)),
        Some(anchor1 + Duration::minutes(30))
    );

    // An unretained pin is the 404 shape (ReferenceTimeNotFound), matching
    // the GRIB/QueryData convention — not a generic engine error.
    let gone = anchor1 - Duration::hours(6);
    let err = match engine.get_raster_tile(
        EXTENT,
        W,
        H,
        None,
        &OutputCrs::Wgs84,
        None,
        None,
        Some(gone),
    ) {
        Err(e) => e,
        Ok(_) => panic!("unretained generation must error"),
    };
    assert!(
        matches!(
            err,
            ds_core::error::DataServerError::ReferenceTimeNotFound(_)
        ),
        "expected ReferenceTimeNotFound, got: {err}"
    );
}

/// No silent caps: a horizon/step pair exceeding the per-generation lead cap
/// is a config error at construction, not a silently shortened horizon.
#[test]
fn excessive_lead_count_is_rejected_at_construction() {
    let source = Arc::new(MockSource {
        times: RwLock::new(vec![t0()]),
    });
    let config = NowcastConfig {
        source: "mock".into(),
        horizon: "PT2H".into(),
        step: Some("PT1M".into()), // 120 leads > cap
        history_frames: 2,
        poll_interval_secs: 30,
        max_generations: 4,
        max_pixels: 4_000_000,
        min_echo: 10.0,
    };
    let err = NowcastEngine::new("mock-nowcast", "mock", source, &config)
        .err()
        .expect("must reject a lead count over the cap");
    assert!(
        err.to_string().contains("exceeds the cap"),
        "error should name the cap: {err}"
    );
}

/// Sub-second source cadence must fail the generation cleanly instead of
/// producing Infinity lead intervals (which would explode advection's
/// substep count and wedge the poll runtime).
#[test]
fn sub_second_cadence_fails_generation_cleanly() {
    let t1 = t0() + Duration::milliseconds(500);
    let (_source, engine) = build("PT1H", &[t0(), t1]);
    engine.poll_once();
    assert!(!engine.has_data(), "sub-second cadence must not generate");
    let (generations, failures, ..) = engine.metrics();
    assert_eq!(generations, 0);
    assert_eq!(failures, 1, "the failure must be counted, not hidden");
}

/// history_frames is bounded: each frame is a blocking source fetch per
/// generation, so an oversized value is a config error, not a silent cap.
#[test]
fn oversized_history_frames_is_rejected() {
    let source = Arc::new(MockSource {
        times: RwLock::new(vec![t0()]),
    });
    let config = NowcastConfig {
        source: "mock".into(),
        horizon: "PT1H".into(),
        step: None,
        history_frames: 24,
        poll_interval_secs: 30,
        max_generations: 4,
        max_pixels: 4_000_000,
        min_echo: 10.0,
    };
    let err = NowcastEngine::new("mock-nowcast", "mock", source, &config)
        .err()
        .expect("must reject oversized history_frames");
    assert!(err.to_string().contains("exceeds the cap"), "got: {err}");
}

/// V2.1 (#542): each new generation scores the previous one's prediction for
/// the fresh analysis against persistence. On a pure translation the
/// extrapolation reconstructs the truth almost exactly, so its realized CSI
/// must be high and at least match persistence.
#[test]
fn per_generation_skill_is_scored_against_persistence() {
    let anchor1 = t0() + Duration::minutes(5);
    let (source, engine) = build("PT1H", &[t0(), anchor1]);
    engine.poll_once();
    assert!(
        engine.skill_permille().is_none(),
        "no skill before a second generation exists"
    );

    let anchor2 = anchor1 + Duration::minutes(5);
    source.times.write().unwrap().push(anchor2);
    engine.poll_once();
    let (csi, persistence) = engine
        .skill_permille()
        .expect("second generation must produce a skill measurement");
    assert!(
        csi >= persistence,
        "translation nowcast CSI ({csi}) must be >= persistence ({persistence})"
    );
    assert!(csi > 900, "near-perfect reconstruction expected, got {csi}");
}

/// Strict lead-1 gauge semantics (#543 round 3): when a generation is
/// skipped (source cadence gap), the previous generation's match for the
/// new anchor sits at a deeper lead — the lead1 gauges must then stay
/// unset rather than mislabel the measurement.
#[test]
fn skipped_generation_does_not_mislabel_lead1_skill() {
    let anchor1 = t0() + Duration::minutes(5);
    let (source, engine) = build("PT1H", &[t0(), anchor1]);
    engine.poll_once();

    // Source skips 12:10 entirely; next frame is 12:15 — the previous
    // generation's prediction for it is lead 2, not lead 1.
    let anchor3 = anchor1 + Duration::minutes(10);
    source.times.write().unwrap().push(anchor3);
    engine.poll_once();
    assert!(engine.has_data());
    assert_eq!(
        engine.skill_permille(),
        None,
        "a lead-2 match must not populate the lead-1 gauges"
    );
}

/// The skill gauges update as a pair from one frame comparison (#543 round
/// 4): a scene with no scoreable echo must leave both unset — never one
/// updated against the other's stale value.
#[test]
fn dry_scene_leaves_both_skill_gauges_unset() {
    // Frames whose echo (~ -14 dBZ raw 40) never reaches min_echo = 10 dBZ:
    // every contingency denominator is 0 on both sides.
    struct DrySource {
        times: RwLock<Vec<DateTime<Utc>>>,
    }
    impl MapEngine for DrySource {
        fn get_raster_tile(
            &self,
            _bbox: [f64; 4],
            width: u32,
            height: u32,
            _time: Option<DateTime<Utc>>,
            _output_crs: &OutputCrs,
            _parameter: Option<&str>,
            _z: Option<f64>,
            _reference_time: Option<DateTime<Utc>>,
        ) -> Result<RasterTile, DataServerError> {
            Ok(RasterTile {
                width,
                height,
                values: RasterValues::U8 {
                    data: vec![40u8; (width * height) as usize],
                    nodata: Some(NODATA),
                    gain: 0.4,
                    offset: -30.0,
                },
            })
        }
        fn raster_info(&self) -> RasterInfo {
            RasterInfo {
                native_crs: "CRS:84".into(),
                spatial_extent: Some(EXTENT),
                times: self.times.read().unwrap().clone(),
                parameter: "reflectivity".into(),
                unit: "dBZ".into(),
                parameters: vec![],
                vertical: None,
                grid_size: Some([W, H]),
                layer_subtitle: None,
                reference_times: Vec::new(),
            }
        }
    }

    let anchor1 = t0() + Duration::minutes(5);
    let source = Arc::new(DrySource {
        times: RwLock::new(vec![t0(), anchor1]),
    });
    let config = NowcastConfig {
        source: "mock".into(),
        horizon: "PT30M".into(),
        step: None,
        history_frames: 2,
        poll_interval_secs: 30,
        max_generations: 4,
        max_pixels: 4_000_000,
        min_echo: 10.0,
    };
    let engine =
        NowcastEngine::new("dry-nowcast", "mock", source.clone(), &config).expect("engine builds");
    engine.poll_once();
    source
        .times
        .write()
        .unwrap()
        .push(anchor1 + Duration::minutes(5));
    engine.poll_once();
    assert!(engine.has_data());
    assert_eq!(
        engine.skill_permille(),
        None,
        "a dry scene must leave the gauge pair unset"
    );
}
