//! The shared sampler, `ds_core::resample::SourceGrid` (#1067), pinned to
//! this engine's own map render on the MEPS fixture (Lambert conformal
//! conic, rows stored south first): the same decoded field through both
//! gives the same pixels, value for value.

use super::*;
use crate::parse::MISSING_VALUE;
use ds_core::geo::{projected_envelope, projected_output_crs, wgs84_envelope};
use ds_core::map_engine::RasterValues;
use ds_core::resample::SourceGrid;

fn meps() -> (QueryDataEngine, Arc<QueryData>) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/meps");
    let engine = QueryDataEngine::new(&dir, "test", None, 30, 4).unwrap();
    let data = engine.latest_data().unwrap();
    (engine, data)
}

/// One parameter at one step, row-major north first, missing values as
/// QueryData's own sentinel.
fn field(data: &QueryData, param: usize, time: usize) -> Vec<f32> {
    let (nx, ny) = (data.grid.nx as usize, data.grid.ny as usize);
    let mut values = Vec::with_capacity(nx * ny);
    for row in (0..ny).rev() {
        for col in 0..nx {
            let v = data.value(param, row * nx + col, 0, time);
            values.push(v.map_or(MISSING_VALUE, |v| v as f32));
        }
    }
    values
}

/// The projected request for a WGS84 box: its EPSG metres and the WGS84
/// envelope the API layers pass alongside.
fn projected(epsg: &str, wgs84: [f64; 4]) -> ([f64; 4], OutputCrs) {
    let crs = projected_output_crs(epsg).unwrap();
    let bbox = projected_envelope(&crs, wgs84);
    let read = wgs84_envelope(&crs, bbox).unwrap();
    (read, OutputCrs::Projected { crs, bbox })
}

#[test]
fn shared_sampler_renders_meps_as_the_engine_does() {
    let (engine, data) = meps();
    let grid = SourceGrid::new(data.grid.geo_transform().clone());
    let time = data.times.len() - 1;
    let mut views = vec![
        ([10.0, 60.5, 19.0, 64.5], 256, 256, OutputCrs::Wgs84),
        ([10.0, 60.5, 19.0, 64.5], 256, 256, OutputCrs::WebMercator),
        ([5.0, 57.0, 25.0, 68.0], 300, 200, OutputCrs::Wgs84),
        ([14.0, 61.8, 14.6, 62.2], 64, 48, OutputCrs::WebMercator),
        ([-180.0, -90.0, 180.0, 90.0], 512, 256, OutputCrs::Wgs84),
        (
            [-180.0, -85.0, 180.0, 85.0],
            256,
            256,
            OutputCrs::WebMercator,
        ),
    ];
    for (epsg, wgs84) in [
        ("EPSG:3067", [8.0, 59.0, 21.0, 66.0]),
        ("EPSG:3035", [8.0, 59.0, 21.0, 66.0]),
    ] {
        let (read, crs) = projected(epsg, wgs84);
        views.push((read, 200, 220, crs));
    }
    // An Arctic EPSG:3035 view: the pole, and the inverse's longitude cut
    // along 170°W.
    let crs = projected_output_crs("EPSG:3035").unwrap();
    let arctic = [2_000_000.0, 3_500_000.0, 7_000_000.0, 8_500_000.0];
    let read = wgs84_envelope(&crs, arctic).unwrap();
    views.push((read, 256, 256, OutputCrs::Projected { crs, bbox: arctic }));
    for param in 0..data.params.len() {
        let name = data.params[param].name.clone();
        let values = field(&data, param, time);
        for (bbox, width, height, crs) in &views {
            let ours = engine
                .get_raster_tile(
                    *bbox,
                    *width,
                    *height,
                    Some(data.times[time]),
                    crs,
                    Some(&name),
                    None,
                    None,
                )
                .unwrap();
            let shared = grid
                .render_f32(&values, Some(MISSING_VALUE), *bbox, *width, *height, crs)
                .unwrap();
            assert!(matches!(shared.values, RasterValues::F32 { .. }));
            let mut filled = 0;
            for i in 0..ours.values.len() {
                let want = ours.values.value_at(i).map(|v| v as f32);
                let got = shared.values.value_at(i).map(|v| v as f32);
                assert_eq!(got, want, "{name} {crs:?} {bbox:?} pixel {i}");
                filled += usize::from(got.is_some());
            }
            assert!(filled > 0, "{name} {crs:?} {bbox:?}: no data");
        }
    }
}
