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

/// Lightning join (#549): a mock event source dropping a fixed burst on
/// the disc each window must surface flash properties on the cell —
/// and an engine WITHOUT a source must not emit them at all.
#[test]
fn lightning_join_exposes_flash_properties() {
    use ds_core::events::{EventPoint, EventSource};
    use ds_core::feature::{FeatureQuery, PropertyValue};
    use ds_core::feature_engine::FeatureEngine;

    struct DiscStrikes;
    impl EventSource for DiscStrikes {
        fn recent_events(
            &self,
            _start: DateTime<Utc>,
            end: DateTime<Utc>,
            _limit: usize,
        ) -> Result<Vec<EventPoint>, ds_core::error::DataServerError> {
            // disc_center is in grid PIXELS; strikes arrive in WGS84.
            let (cx, cy) = disc_center(end);
            let lon = EXTENT[0] + cx / f64::from(W) * (EXTENT[2] - EXTENT[0]);
            let lat = EXTENT[3] - cy / f64::from(H) * (EXTENT[3] - EXTENT[1]);
            Ok(vec![
                EventPoint {
                    time: end,
                    lon,
                    lat
                };
                30
            ])
        }
    }

    let anchor1 = t0() + Duration::minutes(5);
    let source = Arc::new(MockSource {
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
        growth_decay: false,
        lightning_source: Some("mock-lightning".into()),
        significance: Default::default(),
        impact_source: None,
        impact_name_property: "name".into(),
        impact_weight_property: None,
    };
    let engine = NowcastEngine::new("lj-nowcast", "mock", source.clone(), &config)
        .expect("engine builds")
        .with_lightning_source(Arc::new(DiscStrikes));
    engine.poll_once();
    let page = engine.get_features(&FeatureQuery::default()).unwrap();
    let f = &page.features[0];
    assert!(matches!(
        f.properties.get("flash_count"),
        Some(PropertyValue::Integer(30))
    ));
    // 30 strikes over the 5-min window = 6 flashes/min — measured, but
    // under the 10/min jump floor: never a jump, even with a baseline.
    assert!(matches!(
        f.properties.get("flash_rate_per_min"),
        Some(PropertyValue::Float(r)) if (r - 6.0).abs() < 1e-6
    ));
    assert!(matches!(
        f.properties.get("lightning_jump"),
        Some(PropertyValue::Bool(false))
    ));

    let anchor2 = anchor1 + Duration::minutes(5);
    source.times.write().unwrap().push(anchor2);
    engine.poll_once();
    let page = engine.get_features(&FeatureQuery::default()).unwrap();
    let f = &page.features[0];
    assert!(matches!(
        f.properties.get("flash_count"),
        Some(PropertyValue::Integer(30))
    ));
    assert!(matches!(
        f.properties.get("lightning_jump"),
        Some(PropertyValue::Bool(false))
    ));

    // No source wired ⇒ the flash properties do not exist (absent, not
    // null — "not measured" is a different statement than "no strikes").
    let (_s2, plain) = build("PT30M", &[t0(), anchor1]);
    plain.poll_once();
    let page = plain.get_features(&FeatureQuery::default()).unwrap();
    assert!(!page.features[0].properties.contains_key("flash_count"));
    assert!(!page.features[0].properties.contains_key("lightning_jump"));
}

/// A failing event source degrades to null flash fields for that
/// generation — the generation itself, and the properties' presence,
/// survive (#549 contract: present-but-null = "join skipped").
#[test]
fn lightning_source_error_degrades_to_null_fields() {
    use ds_core::events::{EventPoint, EventSource};
    use ds_core::feature::{FeatureQuery, PropertyValue};
    use ds_core::feature_engine::FeatureEngine;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FlakyStrikes {
        fail: AtomicBool,
    }
    impl EventSource for FlakyStrikes {
        fn recent_events(
            &self,
            _start: DateTime<Utc>,
            end: DateTime<Utc>,
            _limit: usize,
        ) -> Result<Vec<EventPoint>, ds_core::error::DataServerError> {
            if self.fail.load(Ordering::Relaxed) {
                return Err(ds_core::error::DataServerError::Engine(
                    "lightning db unreachable".into(),
                ));
            }
            let (cx, cy) = disc_center(end);
            let lon = EXTENT[0] + cx / f64::from(W) * (EXTENT[2] - EXTENT[0]);
            let lat = EXTENT[3] - cy / f64::from(H) * (EXTENT[3] - EXTENT[1]);
            Ok(vec![
                EventPoint {
                    time: end,
                    lon,
                    lat
                };
                10
            ])
        }
    }

    let anchor1 = t0() + Duration::minutes(5);
    let source = Arc::new(MockSource {
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
        growth_decay: false,
        lightning_source: Some("mock-lightning".into()),
        significance: Default::default(),
        impact_source: None,
        impact_name_property: "name".into(),
        impact_weight_property: None,
    };
    let strikes = Arc::new(FlakyStrikes {
        fail: AtomicBool::new(false),
    });
    let engine = NowcastEngine::new("flaky-nowcast", "mock", source.clone(), &config)
        .expect("engine builds")
        .with_lightning_source(strikes.clone());

    // Healthy generation: measured values.
    engine.poll_once();
    let page = engine.get_features(&FeatureQuery::default()).unwrap();
    assert!(matches!(
        page.features[0].properties.get("flash_count"),
        Some(PropertyValue::Integer(10))
    ));

    // Source down for the next generation: it still completes, the track
    // persists, and every flash field reads null — including the jump
    // flag (unknown ≠ false).
    strikes.fail.store(true, Ordering::Relaxed);
    let anchor2 = anchor1 + Duration::minutes(5);
    source.times.write().unwrap().push(anchor2);
    engine.poll_once();
    let page = engine.get_features(&FeatureQuery::default()).unwrap();
    let f = &page.features[0];
    assert!(matches!(
        f.properties.get("track_age"),
        Some(PropertyValue::Integer(2))
    ));
    assert!(matches!(
        f.properties.get("flash_count"),
        Some(PropertyValue::Null)
    ));
    assert!(matches!(
        f.properties.get("flash_rate_per_min"),
        Some(PropertyValue::Null)
    ));
    assert!(matches!(
        f.properties.get("lightning_jump"),
        Some(PropertyValue::Null)
    ));
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
        growth_decay: false,
        lightning_source: None,
        significance: Default::default(),
        impact_source: None,
        impact_name_property: "name".into(),
        impact_weight_property: None,
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
        growth_decay: false,
        lightning_source: None,
        significance: Default::default(),
        impact_source: None,
        impact_name_property: "name".into(),
        impact_weight_property: None,
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
        growth_decay: false,
        lightning_source: None,
        significance: Default::default(),
        impact_source: None,
        impact_name_property: "name".into(),
        impact_weight_property: None,
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
        growth_decay: false,
        lightning_source: None,
        significance: Default::default(),
        impact_source: None,
        impact_name_property: "name".into(),
        impact_weight_property: None,
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

/// V2.2 (#544): after a generation, tracked cells serve as Point features
/// with severity/motion/deviant properties; ids persist across generations.
#[test]
fn cell_features_are_served_and_tracks_persist() {
    use ds_core::feature::{FeatureQuery, PropertyValue};
    use ds_core::feature_engine::FeatureEngine;

    let anchor1 = t0() + Duration::minutes(5);
    let (source, engine) = build("PT30M", &[t0(), anchor1]);
    engine.poll_once();
    let page = engine.get_features(&FeatureQuery::default()).unwrap();
    assert_eq!(page.number_matched, 1, "one disc, one cell");
    let f = &page.features[0];
    assert!(matches!(
        f.properties.get("severity"),
        Some(PropertyValue::String(s)) if s == "moderate"
    ));
    assert!(matches!(
        f.properties.get("deviant_mover"),
        Some(PropertyValue::Bool(false))
    ));
    let id1 = f.id.clone();

    let anchor2 = anchor1 + Duration::minutes(5);
    source.times.write().unwrap().push(anchor2);
    engine.poll_once();
    let page = engine.get_features(&FeatureQuery::default()).unwrap();
    let f = &page.features[0];
    assert_eq!(f.id, id1, "track id persists across generations");
    assert!(matches!(
        f.properties.get("track_age"),
        Some(PropertyValue::Integer(2))
    ));
    assert!(engine.get_feature(&id1).is_ok());
    assert!(engine.get_feature("9999").is_err());

    // Precision contract: values are emitted at meaningful precision,
    // not f64 bit depth (speed 0.1 m/s, bearing whole degrees in
    // [0, 360), area 0.1 km², coordinates 1e-5 deg).
    let frac_ok = |v: f64, f: f64| (v * f - (v * f).round()).abs() < 1e-9;
    match f.properties.get("speed_ms") {
        Some(PropertyValue::Float(v)) => assert!(frac_ok(*v, 10.0), "speed {v}"),
        other => panic!("age-2 cell must report speed, got {other:?}"),
    }
    match f.properties.get("bearing_deg") {
        Some(PropertyValue::Float(b)) => {
            assert!(frac_ok(*b, 1.0) && (0.0..360.0).contains(b), "bearing {b}")
        }
        other => panic!("age-2 cell must report bearing, got {other:?}"),
    }
    match f.properties.get("area_km2") {
        Some(PropertyValue::Float(a)) => assert!(frac_ok(*a, 10.0), "area {a}"),
        other => panic!("missing area, got {other:?}"),
    }
    let ds_core::feature::Geometry::Point { x, y } = *f.geometry else {
        panic!("cell geometry must be a Point");
    };
    assert!(frac_ok(x, 1e5) && frac_ok(y, 1e5), "coords {x},{y}");

    // History (#548): an interval before the latest snapshot selects the
    // OLDER retained snapshot (age-1 cells, observed at anchor1); an
    // interval before all snapshots matches nothing.
    use ds_core::feature::{DatetimeInterval, PropertyValue as PV};
    let hist = engine
        .get_features(&FeatureQuery {
            datetime: Some(DatetimeInterval {
                start: None,
                end: Some(anchor2 - Duration::minutes(1)),
            }),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hist.number_matched, 1, "older snapshot must serve history");
    assert!(matches!(
        hist.features[0].properties.get("track_age"),
        Some(PV::Integer(1))
    ));
    let none = engine
        .get_features(&FeatureQuery {
            datetime: Some(DatetimeInterval {
                start: None,
                end: Some(t0() - Duration::minutes(1)),
            }),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(none.number_matched, 0, "before all snapshots: nothing");
    assert_eq!(engine.feature_count(), 1);
}

/// Geometry-change reset (#545 round 3/4): a source that changes its
/// advertised extent mid-run must restart tracks as newborns instead of
/// matching centroids reinterpreted on the new grid.
#[test]
fn geometry_change_resets_cell_tracks() {
    struct MovableSource {
        times: RwLock<Vec<DateTime<Utc>>>,
        extent: RwLock<[f64; 4]>,
    }
    impl MapEngine for MovableSource {
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
            Ok(RasterTile {
                width,
                height,
                values: RasterValues::U8 {
                    data: truth_frame(time.unwrap()),
                    nodata: Some(NODATA),
                    gain: 0.4,
                    offset: -30.0,
                },
            })
        }
        fn raster_info(&self) -> RasterInfo {
            RasterInfo {
                native_crs: "CRS:84".into(),
                spatial_extent: Some(*self.extent.read().unwrap()),
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
    use ds_core::feature::{FeatureQuery, PropertyValue};
    use ds_core::feature_engine::FeatureEngine;

    let anchor1 = t0() + Duration::minutes(5);
    let source = Arc::new(MovableSource {
        times: RwLock::new(vec![t0(), anchor1]),
        extent: RwLock::new(EXTENT),
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
        growth_decay: false,
        lightning_source: None,
        significance: Default::default(),
        impact_source: None,
        impact_name_property: "name".into(),
        impact_weight_property: None,
    };
    let engine =
        NowcastEngine::new("mv-nowcast", "mock", source.clone(), &config).expect("engine builds");
    engine.poll_once();

    // Extent shifts (source footprint change) before the next frame.
    *source.extent.write().unwrap() = [1.0, 51.0, 11.0, 61.0];
    source
        .times
        .write()
        .unwrap()
        .push(anchor1 + Duration::minutes(5));
    engine.poll_once();
    let page = engine.get_features(&FeatureQuery::default()).unwrap();
    assert_eq!(page.number_matched, 1);
    assert!(
        matches!(
            page.features[0].properties.get("track_age"),
            Some(PropertyValue::Integer(1))
        ),
        "track must restart as newborn after a geometry change"
    );
}

/// Growth/decay application (#546): a source whose echo fades every frame
/// must produce dimmer lead frames than pure advection when enabled.
#[test]
fn growth_decay_dims_decaying_echo() {
    struct FadingSource {
        times: RwLock<Vec<DateTime<Utc>>>,
    }
    impl MapEngine for FadingSource {
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
            // Static disc fading 1 dBZ (2.5 raw) per 5-min frame.
            let t = time.unwrap();
            let steps = ((t - t0()).num_seconds() / 300) as f64;
            let raw_val = (175.0 - 2.5 * steps).max(100.0) as u8;
            let data: Vec<u8> = truth_frame(t0())
                .into_iter()
                .map(|r| if r == ECHO_RAW { raw_val } else { r })
                .collect();
            Ok(RasterTile {
                width,
                height,
                values: RasterValues::U8 {
                    data,
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
    let run = |growth_decay: bool| -> f32 {
        let anchor = t0() + Duration::minutes(5);
        let source = Arc::new(FadingSource {
            times: RwLock::new(vec![t0(), anchor]),
        });
        let config = NowcastConfig {
            source: "mock".into(),
            horizon: "PT1H".into(),
            step: None,
            history_frames: 2,
            poll_interval_secs: 30,
            max_generations: 4,
            max_pixels: 4_000_000,
            min_echo: 10.0,
            growth_decay,
            lightning_source: None,
            significance: Default::default(),
            impact_source: None,
            impact_name_property: "name".into(),
            impact_weight_property: None,
        };
        let engine = NowcastEngine::new("fade", "mock", source.clone(), &config).expect("builds");
        engine.poll_once();
        // Per-cell tendencies need a matched predecessor: second generation.
        let anchor2 = anchor + Duration::minutes(5);
        source.times.write().unwrap().push(anchor2);
        engine.poll_once();
        let raw = render_raw(&engine, anchor2 + Duration::minutes(30));
        raw.iter()
            .filter(|&&r| r != NODATA && r > 0)
            .map(|&r| r as f32 * 0.4 - 30.0)
            .fold(f32::MIN, f32::max)
    };
    let plain = run(false);
    let adjusted = run(true);
    assert!(
        adjusted < plain - 1.0,
        "growth/decay must dim a fading echo at +30 min: {adjusted} vs {plain}"
    );
}

/// Significance ranking: a scene with two very different cells must serve
/// both, rank the dangerous one first, and say WHY.
///
/// The scene is the operationally interesting one — a big intense core and a
/// small weak blob — because a ranking that cannot separate those is not
/// worth serving.
#[test]
fn cells_are_ranked_by_significance_with_reasons() {
    use ds_core::feature::{FeatureQuery, PropertyValue};
    use ds_core::feature_engine::FeatureEngine;

    /// Raw byte for ~57 dBZ under gain 0.4 / offset −30 (crosses all three
    /// severity steps); `ECHO_RAW` is ~40 dBZ and crosses none.
    const STRONG_RAW: u8 = 218;

    struct TwoCellSource {
        times: RwLock<Vec<DateTime<Utc>>>,
    }

    impl MapEngine for TwoCellSource {
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
            let mut data = vec![0u8; (width * height) as usize];
            for (i, cell) in data.iter_mut().enumerate() {
                let x = (i % width as usize) as f64 + 0.5;
                let y = (i / width as usize) as f64 + 0.5;
                // Weak, small.
                if (x - 40.0).powi(2) + (y - 50.0).powi(2) <= 8.0 * 8.0 {
                    *cell = ECHO_RAW;
                }
                // Intense, large — and far enough away to stay a separate
                // connected component.
                if (x - 140.0).powi(2) + (y - 140.0).powi(2) <= 20.0 * 20.0 {
                    *cell = STRONG_RAW;
                }
            }
            Ok(RasterTile {
                width,
                height,
                values: RasterValues::U8 {
                    data,
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
    let source = Arc::new(TwoCellSource {
        times: RwLock::new(vec![t0(), anchor1]),
    });
    let engine = NowcastEngine::new("ranked", "mock", source, &base_config())
        .expect("engine builds with default weights");
    engine.poll_once();

    let page = engine.get_features(&FeatureQuery::default()).unwrap();
    assert_eq!(page.number_matched, 2, "two discs, two cells");

    let cell = |severity: &str| {
        page.features
            .iter()
            .find(|f| {
                matches!(f.properties.get("severity"),
                    Some(PropertyValue::String(s)) if s == severity)
            })
            .unwrap_or_else(|| panic!("no {severity} cell in {:?}", page.features))
    };
    let strong = cell("very_severe");
    let weak = cell("moderate");

    let score = |f: &ds_core::feature::Feature| match f.properties.get("significance") {
        Some(PropertyValue::Float(v)) => *v,
        other => panic!("missing significance, got {other:?}"),
    };
    let rank = |f: &ds_core::feature::Feature| match f.properties.get("significance_rank") {
        Some(PropertyValue::Integer(v)) => *v,
        other => panic!("missing significance_rank, got {other:?}"),
    };

    assert_eq!(rank(strong), 1, "the intense cell must rank first");
    assert_eq!(rank(weak), 2);
    assert!(
        score(strong) > score(weak),
        "scores must order the same way as ranks: {} vs {}",
        score(strong),
        score(weak)
    );
    for f in &page.features {
        let s = score(f);
        assert!((0.0..=1.0).contains(&s), "significance out of range: {s}");
    }

    // A ranking nobody can argue with is one nobody can tune.
    match strong.properties.get("significance_reasons") {
        Some(PropertyValue::List(reasons)) => {
            assert!(!reasons.is_empty(), "ranked cell must explain itself");
            assert!(
                reasons.len() <= 3,
                "reasons are a summary, not the whole table"
            );
            assert!(reasons
                .iter()
                .all(|r| matches!(r, PropertyValue::String(_))));
        }
        other => panic!("missing significance_reasons, got {other:?}"),
    }
}

/// A typo in a `[nowcast.significance]` weight name must fail the collection
/// at load, not silently rank by defaults the operator never chose.
#[test]
fn unknown_significance_weight_fails_the_collection() {
    let mut config = base_config();
    config.significance.insert("max_dbzz".into(), 2.0);
    let source = Arc::new(MockSource {
        times: RwLock::new(vec![t0()]),
    });
    let err = match NowcastEngine::new("bad-weights", "mock", source, &config) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("a misspelled weight must be rejected"),
    };
    assert!(
        err.contains("max_dbzz"),
        "error should name the typo: {err}"
    );
    assert!(
        err.contains("max_dbz"),
        "error should list valid names: {err}"
    );

    // And the correctly spelled one is accepted.
    let mut good = base_config();
    good.significance.insert("max_dbz".into(), 2.0);
    let source = Arc::new(MockSource {
        times: RwLock::new(vec![t0()]),
    });
    assert!(NowcastEngine::new("good-weights", "mock", source, &good).is_ok());
}

/// Shared default config for the ranking tests.
fn base_config() -> NowcastConfig {
    NowcastConfig {
        source: "mock".into(),
        horizon: "PT30M".into(),
        step: None,
        history_frames: 2,
        poll_interval_secs: 30,
        max_generations: 4,
        max_pixels: 4_000_000,
        min_echo: 10.0,
        growth_decay: false,
        lightning_source: None,
        significance: Default::default(),
        impact_source: None,
        impact_name_property: "name".into(),
        impact_weight_property: None,
    }
}

/// Impact context (Phase 2): a weaker cell over a populated area must
/// outrank a stronger one over nothing.
///
/// This is the whole point of the impact term — radar attributes answer "how
/// intense", only impact answers "does anyone care". If this inverts, the
/// ranking has regressed to sorting by reflectivity.
#[test]
fn impact_context_lifts_a_populated_cell_above_a_stronger_one() {
    use ds_core::feature::{Feature, FeaturePage, FeatureQuery, Geometry, PropertyValue};
    use ds_core::feature_engine::FeatureEngine;
    use std::collections::HashMap;

    /// ~57 dBZ under gain 0.4 / offset −30; `ECHO_RAW` is ~40 dBZ.
    const STRONG_RAW: u8 = 218;

    struct TwoCellSource;
    impl MapEngine for TwoCellSource {
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
            let mut data = vec![0u8; (width * height) as usize];
            for (i, cell) in data.iter_mut().enumerate() {
                let x = (i % width as usize) as f64 + 0.5;
                let y = (i / width as usize) as f64 + 0.5;
                // Weak cell at px (40, 50) => lon 2.0, lat 57.5.
                if (x - 40.0).powi(2) + (y - 50.0).powi(2) <= 8.0 * 8.0 {
                    *cell = ECHO_RAW;
                }
                // Strong cell at px (140, 140) => lon 7.0, lat 53.0.
                if (x - 140.0).powi(2) + (y - 140.0).powi(2) <= 20.0 * 20.0 {
                    *cell = STRONG_RAW;
                }
            }
            Ok(RasterTile {
                width,
                height,
                values: RasterValues::U8 {
                    data,
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
                times: vec![t0(), t0() + Duration::minutes(5)],
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

    /// One populated area, covering the WEAK cell only.
    struct Areas;
    impl FeatureEngine for Areas {
        fn get_features(&self, _q: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
            let mut props = HashMap::new();
            props.insert("name".to_string(), PropertyValue::String("Bigtown".into()));
            props.insert("population".to_string(), PropertyValue::Integer(694_392));
            let f = Feature {
                id: "1".into(),
                geometry: Arc::new(Geometry::Polygon {
                    exterior: vec![
                        [1.0, 57.0],
                        [3.0, 57.0],
                        [3.0, 58.0],
                        [1.0, 58.0],
                        [1.0, 57.0],
                    ],
                    holes: vec![],
                }),
                properties: Arc::new(props),
            };
            Ok(FeaturePage {
                number_matched: 1,
                number_returned: 1,
                features: vec![f],
                next_offset: None,
            })
        }
        fn get_feature(&self, _id: &str) -> Result<Feature, DataServerError> {
            unreachable!("impact index only calls get_features")
        }
    }

    let severity_of = |f: &Feature| match f.properties.get("severity") {
        Some(PropertyValue::String(s)) => s.clone(),
        other => panic!("missing severity: {other:?}"),
    };
    let rank_of = |f: &Feature| match f.properties.get("significance_rank") {
        Some(PropertyValue::Integer(v)) => *v,
        other => panic!("missing significance_rank: {other:?}"),
    };

    // Baseline: no impact source at all. Intensity wins, as it must.
    let plain = NowcastEngine::new("plain", "mock", Arc::new(TwoCellSource), &base_config())
        .expect("engine builds");
    plain.poll_once();
    let page = plain.get_features(&FeatureQuery::default()).unwrap();
    assert_eq!(page.number_matched, 2);
    let strong = page
        .features
        .iter()
        .find(|f| severity_of(f) == "very_severe")
        .unwrap();
    assert_eq!(
        rank_of(strong),
        1,
        "without impact context, the strongest cell ranks first"
    );
    assert!(
        strong.properties.get("impact_over").is_none(),
        "impact properties must be ABSENT when no source is wired, not null"
    );

    // Same scene, with a populated area over the WEAK cell only.
    let mut config = base_config();
    config.impact_source = Some("areas".into());
    config.impact_weight_property = Some("population".into());
    let engine = NowcastEngine::new("impacted", "mock", Arc::new(TwoCellSource), &config)
        .expect("engine builds")
        .with_impact_source(Arc::new(Areas), "name", Some("population"));
    engine.poll_once();

    let page = engine.get_features(&FeatureQuery::default()).unwrap();
    assert_eq!(page.number_matched, 2);
    let weak = page
        .features
        .iter()
        .find(|f| severity_of(f) == "moderate")
        .unwrap();
    let strong = page
        .features
        .iter()
        .find(|f| severity_of(f) == "very_severe")
        .unwrap();

    assert!(matches!(
        weak.properties.get("impact_over"),
        Some(PropertyValue::String(s)) if s == "Bigtown"
    ));
    assert!(
        matches!(
            strong.properties.get("impact_over"),
            Some(PropertyValue::Null)
        ),
        "a cell over nothing reports null, not a missing key"
    );
    assert_eq!(
        rank_of(weak),
        1,
        "a moderate cell over a city must outrank a very severe cell over nothing"
    );
    assert_eq!(rank_of(strong), 2);
}

/// #605 / review: an unwired collection must not advertise properties its
/// features don't carry — sorting on one would return 200 in id order.
#[test]
fn unwired_sources_are_not_advertised_as_sortable() {
    use ds_core::feature_engine::FeatureEngine;

    let source = Arc::new(MockSource {
        times: RwLock::new(vec![t0()]),
    });
    let plain = NowcastEngine::new("plain", "mock", source, &base_config()).unwrap();
    let base: Vec<&str> = plain.sortables().to_vec();

    // Not wired ⇒ the conditional properties must NOT be advertised. A
    // property absent from every feature sorts to a no-op, so advertising it
    // would return 200 in id order — the silent-ignore this surface removes.
    for absent in ["flash_count", "flash_rate_per_min", "impact_eta_minutes"] {
        assert!(
            !base.contains(&absent),
            "{absent} must not be sortable without its source"
        );
    }
    assert!(base.contains(&"significance"));
}

/// Wiring a source makes exactly its properties sortable, and nothing else.
#[test]
fn wiring_a_source_adds_exactly_its_sortables() {
    use ds_core::feature::{FeatureQuery, PropertyValue};
    use ds_core::feature_engine::FeatureEngine;
    use std::collections::HashMap;

    struct Areas;
    impl FeatureEngine for Areas {
        fn get_features(
            &self,
            _q: &FeatureQuery,
        ) -> Result<ds_core::feature::FeaturePage, DataServerError> {
            let mut props = HashMap::new();
            props.insert("name".to_string(), PropertyValue::String("X".into()));
            Ok(ds_core::feature::FeaturePage {
                features: vec![ds_core::feature::Feature {
                    id: "1".into(),
                    geometry: Arc::new(ds_core::feature::Geometry::Polygon {
                        exterior: vec![
                            [0.0, 50.0],
                            [10.0, 50.0],
                            [10.0, 60.0],
                            [0.0, 60.0],
                            [0.0, 50.0],
                        ],
                        holes: vec![],
                    }),
                    properties: Arc::new(props),
                }],
                number_matched: 1,
                number_returned: 1,
                next_offset: None,
            })
        }
        fn get_feature(&self, _id: &str) -> Result<ds_core::feature::Feature, DataServerError> {
            unreachable!()
        }
    }

    let mk = || {
        Arc::new(MockSource {
            times: RwLock::new(vec![t0()]),
        })
    };
    let plain = NowcastEngine::new("p", "mock", mk(), &base_config()).unwrap();
    let n = plain.sortables().len();

    let with_impact = NowcastEngine::new("i", "mock", mk(), &base_config())
        .unwrap()
        .with_impact_source(Arc::new(Areas), "name", None);
    assert_eq!(with_impact.sortables().len(), n + 1);
    assert!(with_impact.sortables().contains(&"impact_eta_minutes"));
    assert!(
        !with_impact.sortables().contains(&"flash_count"),
        "an impact source must not advertise lightning properties"
    );
}

/// #614: a fixed echo (wind turbine clutter) outranked real weather on a
/// quiet day. A stationary source produces a cell that never moves, so after
/// enough frames it must be flagged and demoted — while a moving cell of the
/// same intensity is untouched.
#[test]
fn a_stationary_echo_is_flagged_and_demoted() {
    use ds_core::feature::{FeatureQuery, PropertyValue};
    use ds_core::feature_engine::FeatureEngine;

    /// Same disc every frame, never moving — a mast or turbine farm.
    struct FixedEchoSource {
        times: RwLock<Vec<DateTime<Utc>>>,
    }
    impl MapEngine for FixedEchoSource {
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
            let mut data = vec![0u8; (width * height) as usize];
            for (i, cell) in data.iter_mut().enumerate() {
                let x = (i % width as usize) as f64 + 0.5;
                let y = (i / width as usize) as f64 + 0.5;
                if (x - 100.0).powi(2) + (y - 100.0).powi(2) <= 9.0 * 9.0 {
                    *cell = 218; // ~57 dBZ: bright, like real clutter
                }
            }
            Ok(RasterTile {
                width,
                height,
                values: RasterValues::U8 {
                    data,
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

    let source = Arc::new(FixedEchoSource {
        times: RwLock::new(vec![t0(), t0() + Duration::minutes(5)]),
    });
    let engine =
        NowcastEngine::new("fixed", "mock", source.clone(), &base_config()).expect("builds");

    // Walk enough generations for the track to become persistent.
    let mut t = t0() + Duration::minutes(5);
    for _ in 0..8 {
        engine.poll_once();
        t += Duration::minutes(5);
        source.times.write().unwrap().push(t);
    }
    engine.poll_once();

    let page = engine.get_features(&FeatureQuery::default()).unwrap();
    let f = page.features.first().expect("the fixed echo is tracked");
    let p = &f.properties;

    // It never moved, so it is not weather.
    match p.get("speed_ms") {
        Some(PropertyValue::Float(v)) => assert!(*v < 3.0, "should be stationary, got {v}"),
        other => panic!("expected a measured speed by now, got {other:?}"),
    }
    assert!(
        matches!(p.get("likely_clutter"), Some(PropertyValue::Bool(true))),
        "a persistent stationary echo must be flagged: {p:?}"
    );

    // Flagged, but still present and inspectable — never silently dropped.
    assert_eq!(page.number_matched, 1);
    assert!(matches!(p.get("max_dbz"), Some(PropertyValue::Float(_))));

    // And the demotion actually applied.
    match p.get("significance_reasons") {
        Some(PropertyValue::List(r)) => assert!(
            r.iter()
                .any(|x| matches!(x, PropertyValue::String(s) if s == "clutter")),
            "clutter should be among the top reasons: {r:?}"
        ),
        other => panic!("missing reasons: {other:?}"),
    }
}
