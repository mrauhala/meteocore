//! The retained frames' bytes (`NowcastEngine::frame_bytes`, the
//! `nowcast_frame_bytes` gauge, #1307) against the heap the retained
//! generations hold, measured with a counting allocator. One test: every
//! thread's allocations count.

use std::sync::{Arc, RwLock};

use chrono::{DateTime, Duration, Utc};
use ds_core::config::NowcastConfig;
use ds_core::error::DataServerError;
use ds_core::heap::Counting;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile, RasterValues};
use engine_nowcast::NowcastEngine;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const W: u32 = 480;
const H: u32 = 480;

fn t0() -> DateTime<Utc> {
    "2026-07-20T12:00:00Z".parse().unwrap()
}

/// A disc of echo moving 2 px east per 5-minute frame, as raw bytes.
fn frame(t: DateTime<Utc>) -> Vec<u8> {
    let cx = 120.0 + 2.0 * (t - t0()).num_seconds() as f64 / 300.0;
    let mut data = vec![0u8; (W * H) as usize];
    for (i, cell) in data.iter_mut().enumerate() {
        let x = (i % W as usize) as f64 + 0.5;
        let y = (i / W as usize) as f64 + 0.5;
        if (x - cx).powi(2) + (y - 240.0).powi(2) <= 40.0 * 40.0 {
            *cell = 175;
        }
    }
    data
}

struct Source {
    times: RwLock<Vec<DateTime<Utc>>>,
}

impl MapEngine for Source {
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
        let t = time.ok_or_else(|| DataServerError::Engine("needs a time".into()))?;
        Ok(RasterTile {
            width,
            height,
            values: RasterValues::U8 {
                data: frame(t),
                nodata: Some(255),
                gain: 0.4,
                offset: -30.0,
            },
        })
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: Some([0.0, 50.0, 10.0, 60.0]),
            times: self.times.read().unwrap().clone(),
            parameter: "reflectivity".into(),
            unit: "dBZ".into(),
            parameters: vec![],
            vertical: None,
            grid_size: Some([W, H]),
            layer_subtitle: None,
            reference_times: Vec::new(),
            native_grid: None,
        }
    }
}

#[test]
fn frame_bytes_match_the_heap_the_generations_hold() {
    let config = NowcastConfig {
        source: "radar".into(),
        horizon: "PT1H".into(),
        step: None,
        history_frames: 3,
        poll_interval_secs: 30,
        max_generations: 3,
        max_pixels: 4_000_000,
        min_echo: 10.0,
        growth_decay: false,
        lightning_source: None,
        significance: Default::default(),
        impact_source: None,
        impact_name_property: "name".into(),
        impact_weight_property: None,
        radar_source: None,
    };
    let source = Arc::new(Source {
        times: RwLock::new(vec![t0(), t0() + Duration::minutes(5)]),
    });
    let engine =
        NowcastEngine::new("nowcast", "radar", source.clone(), &config).expect("engine builds");
    assert_eq!(engine.frame_bytes(), 0);

    // Five generations, three retained: the oldest two are dropped again.
    let before = Counting::live();
    for anchor in 1..=5 {
        let times: Vec<_> = (0..=anchor)
            .map(|i| t0() + Duration::minutes(5 * i))
            .collect();
        *source.times.write().unwrap() = times;
        engine.poll_once();
    }
    let held = (Counting::live() - before) as f64;
    let (_, failures, _, _, retained, frames) = engine.metrics();
    assert_eq!((failures, retained), (0, 3));
    // Exact: 1 byte per pixel, the analysis frame and 12 leads, three times.
    assert_eq!(frames, 13);
    assert_eq!(engine.frame_bytes(), 3 * 13 * u64::from(W * H));

    // The frames are what the retained state holds; motion fields, cells
    // and metadata are the rest.
    let estimate = engine.frame_bytes() as f64;
    println!(
        "nowcast: held {held} bytes, frames {estimate} ({:+.1} %)",
        100.0 * (estimate - held) / held
    );
    assert!(
        (estimate - held).abs() <= 0.25 * held,
        "frame bytes {estimate} are more than 25 % off the {held} bytes held"
    );
}
