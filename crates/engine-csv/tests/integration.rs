//! Station-symbol map layers over real CSV fixtures (#1204):
//! `tests/fixtures/station_history.csv` (three stations at whole-degree
//! positions, one of which moves after its first row) for exact pixels, and
//! `testdata/weather.csv` (193 Finnish stations, hourly, January 2026) for
//! a realistic frame. Reference projections from PROJ
//! (`cs2cs EPSG:4326 EPSG:3857` / `EPSG:3035`).

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterTile};
use ds_core::model::CoverageResponse;
use ds_core::station_map::StationMapSettings;
use ds_render::station_symbols::{StationSymbols, NODATA_MARKER};
use engine_csv::{CsvDataStore, CsvEngine};

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

fn history(settings: StationMapSettings) -> CsvEngine {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/station_history.csv"
    );
    CsvEngine::new(CsvDataStore::load(path).unwrap()).with_station_map(settings)
}

fn weather(settings: StationMapSettings) -> CsvEngine {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/weather.csv");
    CsvEngine::new(CsvDataStore::load(path).unwrap()).with_station_map(settings)
}

fn one_pixel() -> StationMapSettings {
    StationMapSettings {
        marker_size: 1,
        ..Default::default()
    }
}

fn render(
    engine: &CsvEngine,
    bbox: [f64; 4],
    size: (u32, u32),
    time: Option<&str>,
    crs: &OutputCrs,
    parameter: &str,
) -> RasterTile {
    engine
        .get_raster_tile(
            bbox,
            size.0,
            size.1,
            time.map(at),
            crs,
            Some(parameter),
            None,
            None,
        )
        .unwrap()
}

/// The painted pixels of `tile` and their values, as `(column, row)`.
fn painted(tile: &RasterTile) -> HashMap<(u32, u32), f64> {
    (0..tile.height)
        .flat_map(|row| (0..tile.width).map(move |col| (col, row)))
        .filter_map(|(col, row)| {
            tile.values
                .value_at((row * tile.width + col) as usize)
                .map(|v| ((col, row), v))
        })
        .collect()
}

/// One layer per column with the unit the EDR descriptions carry, frames
/// every 10 minutes over the file's last 6 h, the render's selection for
/// `resolve_time` and `resolve_parameter_time` (#507), a static content
/// version, and no GetFeatureInfo.
#[test]
fn layers_advertise_each_column_on_ten_minute_frames() {
    let engine = weather(StationMapSettings::default());
    let info = engine.raster_info_shared();
    let layers: Vec<(&str, &str)> = info
        .parameters
        .iter()
        .map(|p| (p.name.as_str(), p.unit.as_str()))
        .collect();
    assert_eq!(
        layers,
        [
            ("temperature", "°C"),
            ("humidity", "%"),
            ("wind_speed", "m/s")
        ]
    );
    assert_eq!(info.parameter, "temperature");
    assert_eq!(info.native_crs, "CRS:84");
    assert_eq!(info.times.len(), 37);
    assert_eq!(info.times.first(), Some(&at("2026-01-31T17:00:00Z")));
    assert_eq!(info.times.last(), Some(&at("2026-01-31T23:00:00Z")));
    assert_eq!(
        engine.resolve_time(None, None),
        Some(at("2026-01-31T23:00:00Z"))
    );
    for parameter in [None, Some("humidity")] {
        assert_eq!(
            engine.resolve_parameter_time(parameter, Some(at("2026-01-15T12:09:59Z")), None),
            Some(at("2026-01-15T12:00:00Z"))
        );
    }
    assert_eq!(engine.content_version(), 0);
    assert!(!engine.layer_shows_values("temperature"));
    assert!(matches!(
        engine.get_raster_tile(
            [19.0, 59.0, 32.0, 71.0],
            10,
            10,
            None,
            &OutputCrs::Wgs84,
            Some("nope"),
            None,
            None
        ),
        Err(DataServerError::InvalidParameter(_))
    ));
}

/// CRS:84 is linear in degrees. 0.1° a pixel over a box offset by half a
/// pixel, so each whole-degree station is a pixel centre: zeta (24°E 60°N,
/// its first row; it moves to 30°E 70°N later) is column 4, row 15. Its
/// marker holds its latest value, 3, at its inventory position.
#[test]
fn markers_land_on_their_pixels_in_crs84() {
    let engine = history(one_pixel());
    let tile = render(
        &engine,
        [23.55, 59.55, 26.55, 61.55],
        (30, 20),
        Some("2026-01-01T01:00:00Z"),
        &OutputCrs::Wgs84,
        "temperature",
    );
    assert_eq!(
        painted(&tile),
        HashMap::from([((4, 15), 3.0), ((14, 5), 5.0), ((24, 15), 4.0)])
    );
}

/// EPSG:3857 rows follow Mercator northing. Over 0°–62°N in 100 rows, 60°N
/// is row 3 in CRS:84 but row 5 in Web Mercator, 61°N row 1 against 2.
/// PROJ northings: 62°N 8 859 142.8006 m, 61°N 8 625 823.2002 m, 60°N
/// 8 399 737.8898 m.
#[test]
fn markers_land_on_their_pixels_in_epsg3857() {
    let top = 8_859_142.800_566;
    let row = |northing: f64| ((top - northing) / top * 100.0).floor() as u32;
    let (row60, row61) = (row(8_399_737.889_818), row(8_625_823.200_204));
    assert_eq!((row60, row61), (5, 2), "the reference itself");
    let engine = history(one_pixel());
    let bbox = [23.55, 0.0, 26.55, 62.0];
    let time = Some("2026-01-01T01:00:00Z");
    let mercator = render(
        &engine,
        bbox,
        (30, 100),
        time,
        &OutputCrs::WebMercator,
        "temperature",
    );
    assert_eq!(
        painted(&mercator),
        HashMap::from([((4, row60), 3.0), ((14, row61), 5.0), ((24, row60), 4.0)])
    );
    let linear = render(
        &engine,
        bbox,
        (30, 100),
        time,
        &OutputCrs::Wgs84,
        "temperature",
    );
    assert_eq!(
        painted(&linear),
        HashMap::from([((4, 3), 3.0), ((14, 1), 5.0), ((24, 3), 4.0)])
    );
}

/// EPSG:3035 lays pixels out in its own metres: alpha (25°E 61°N) is
/// (5 128 072.7031 E, 4 297 135.9889 N). 100 m pixels with alpha at the
/// centre of pixel (30, 70); the other stations are 50 km and more away.
#[test]
fn markers_land_on_their_pixels_in_epsg3035() {
    let crs = ds_core::geo::projected_output_crs("EPSG:3035").unwrap();
    let (e, n) = (5_128_072.703_1, 4_297_135.988_9);
    let west = e - 3_050.0;
    let north = n + 7_050.0;
    let projected = [west, north - 10_000.0, west + 10_000.0, north];
    let output = OutputCrs::Projected {
        crs: crs.clone(),
        bbox: projected,
    };
    let envelope = ds_core::geo::wgs84_envelope(&crs, projected).unwrap();
    let engine = history(one_pixel());
    let tile = render(
        &engine,
        envelope,
        (100, 100),
        Some("2026-01-01T01:00:00Z"),
        &output,
        "temperature",
    );
    assert_eq!(painted(&tile), HashMap::from([((30, 70), 5.0)]));
}

/// The overlap rule on the CSV layers: three 6-pixel markers on one pixel,
/// the value farthest from the layer's midpoint on top.
#[test]
fn the_most_extreme_value_is_on_top() {
    let engine = history(StationMapSettings::default());
    // 1° pixels: zeta, alpha and beta all cover pixel (5, 4).
    let frame = |engine: &CsvEngine| {
        render(
            engine,
            [20.0, 55.0, 30.0, 65.0],
            (10, 10),
            Some("2026-01-01T01:00:00Z"),
            &OutputCrs::Wgs84,
            "temperature",
        )
        .values
        .value_at(4 * 10 + 5)
    };
    // No midpoint handed over yet: 0, so alpha's 5 is the extreme.
    assert_eq!(frame(&engine), Some(5.0));
    // Midpoint 5: zeta's 3 is now the farthest.
    engine.set_station_symbols(StationSymbols::new(HashMap::from([(
        "temperature".into(),
        5.0,
    )])));
    assert_eq!(frame(&engine), Some(3.0));
}

/// A frame shows each station's latest value in `[TIME - max_age, TIME]`:
/// over 00:30–01:00 beta (00:00 only) has none and is left out, or drawn as
/// a nodata marker with `draw_missing`; zeta and alpha keep their 01:00
/// values.
#[test]
fn a_station_without_a_value_in_the_window_has_no_marker() {
    let bbox = [23.55, 59.55, 26.55, 61.55];
    let short = StationMapSettings {
        max_age: Duration::minutes(30),
        ..one_pixel()
    };
    let time = Some("2026-01-01T01:00:00Z");
    let tile = render(
        &history(short),
        bbox,
        (30, 20),
        time,
        &OutputCrs::Wgs84,
        "temperature",
    );
    assert_eq!(
        painted(&tile),
        HashMap::from([((4, 15), 3.0), ((14, 5), 5.0)])
    );
    let missing = StationMapSettings {
        draw_missing: true,
        ..short
    };
    let tile = render(
        &history(missing),
        bbox,
        (30, 20),
        time,
        &OutputCrs::Wgs84,
        "temperature",
    );
    assert_eq!(painted(&tile)[&(24, 15)], NODATA_MARKER);
    // Before the first row nothing is drawn.
    let before = render(
        &history(one_pixel()),
        bbox,
        (30, 20),
        Some("2025-12-31T23:00:00Z"),
        &OutputCrs::Wgs84,
        "temperature",
    );
    assert!(painted(&before).is_empty());
}

/// On the real network: Helsinki Kaisaniemi's marker at 2026-01-15 12:00Z
/// holds the value its EDR series has then; Maarianhamina Lotsberget never
/// reports a temperature, so its temperature layer has no marker there,
/// while its wind speed layer has one.
#[test]
fn the_weather_network_draws_the_latest_values() {
    let engine = weather(one_pixel());
    let t = at("2026-01-15T12:00:00Z");
    let CoverageResponse::Single(series) = engine
        .query_location(
            "Helsinki Kaisaniemi",
            Some((t, t)),
            Some(&["temperature".to_string()]),
            None,
            None,
        )
        .unwrap()
    else {
        panic!("one coverage");
    };
    let expected = series.ranges["temperature"].values[0].unwrap();
    assert_eq!(expected, -5.5);
    // 0.001° pixels around Kaisaniemi (24.9446°E 60.1752°N): column 44,
    // row 24.
    let bbox = [24.9, 60.15, 25.0, 60.2];
    let tile = render(
        &engine,
        bbox,
        (100, 50),
        Some("2026-01-15T12:00:00Z"),
        &OutputCrs::Wgs84,
        "temperature",
    );
    assert_eq!(painted(&tile), HashMap::from([((44, 24), expected)]));
    // Lotsberget, 19.9355°E 60.0876°N: column 35, row 12 at 0.001°.
    let bbox = [19.9, 60.075, 20.0, 60.1];
    for (parameter, drawn) in [("temperature", false), ("wind_speed", true)] {
        let tile = render(
            &engine,
            bbox,
            (100, 25),
            Some("2026-01-31T23:00:00Z"),
            &OutputCrs::Wgs84,
            parameter,
        );
        assert_eq!(painted(&tile).contains_key(&(35, 12)), drawn, "{parameter}");
    }
}
