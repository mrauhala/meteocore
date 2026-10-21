//! A station grid's terrain and land/sea mask collections (#1206) through
//! `load_collections`: the committed fixtures in
//! `testdata/station-grid-terrain/`, built with GDAL 3.13:
//!
//! - `dem_20000101T0000Z.tif`: a synthetic plane rising to the north-east,
//!   `z = 20 + 30 (lon − 19) + 40 (lat − 59)` m at each 0.05° cell centre
//!   over `[19, 59, 25, 63]`, written as an ESRI ASCII grid and converted
//!   with `gdal_translate -ot Int16 -a_srs EPSG:4326 -a_nodata -32768 -co
//!   TILED=YES -co BLOCKXSIZE=128 -co BLOCKYSIZE=128 -co COMPRESS=DEFLATE
//!   -co PREDICTOR=2`.
//! - `land_20000101T0000Z.tif`: Finland's land, 1 inside and 0 outside,
//!   from `testdata/regions.geojson` (Statistics Finland's regions):
//!   `gdal_rasterize -burn 1 -init 0 -te 19 59 32 71 -tr 0.05 0.05 -ot
//!   Byte`, then `gdal_translate -a_srs EPSG:4326 -a_nodata none -co
//!   TILED=YES -co BLOCKXSIZE=256 -co BLOCKYSIZE=256 -co COMPRESS=DEFLATE`.

use std::collections::HashMap;

use ds_core::config::{CollectionConfig, ServerConfig};

/// The collections of a config whose `[[collections]]` are `body`.
fn collections(body: &str) -> Vec<CollectionConfig> {
    let config: ServerConfig = toml::from_str(&format!(
        "[server]\nhost = \"127.0.0.1\"\nport = 8000\n\n{body}"
    ))
    .expect("test config parses");
    config.validate().expect("test config validates");
    config.collections
}

const WEATHER: &str = r#"
[[collections]]
id = "weather"
title = "Weather"
description = "Stations"
engine_type = "csv"
data_path = "../../testdata/weather.csv"
apis = ["features"]
"#;

const SYNOP: &str = r#"
[[collections]]
id = "synop"
title = "SYNOP"
description = "Stations"
engine_type = "bufr"
apis = ["edr"]
[collections.bufr]
data_path = "../../testdata/bufr-synop"
"#;

/// The terrain lists EDR only: the grid reads its map engine whatever APIs
/// it lists.
const TERRAIN: &str = r#"
[[collections]]
id = "terrain"
title = "Terrain"
description = "Synthetic heights"
engine_type = "geotiff"
data_path = "../../testdata/station-grid-terrain"
apis = ["edr"]
[collections.geotiff]
filename_template = "dem_%Y%m%dT%H%MZ.tif"
parameter = "height"
unit = "m"
"#;

const LAND: &str = r#"
[[collections]]
id = "land"
title = "Land"
description = "Finland's land"
engine_type = "geotiff"
data_path = "../../testdata/station-grid-terrain"
apis = ["wms"]
[collections.geotiff]
filename_template = "land_%Y%m%dT%H%MZ.tif"
parameter = "land"
unit = "1"
"#;

/// A grid over south-west Finland's coast at the fixtures' 0.05°, so its
/// cell centres are their pixel centres.
fn grid(id: &str, source: &str, parameters: &str, extra: &str) -> String {
    format!(
        r#"
[[collections]]
id = "{id}"
title = "{id}"
description = "Analysed."
engine_type = "station-grid"
apis = ["edr", "wms", "maps", "tiles"]
[collections.station_grid]
source = "{source}"
parameters = {parameters}
grid = {{ bbox = [21.0, 60.0, 23.0, 62.0], resolution = 0.05 }}
search_radius_km = 60
cadence = "PT1H"
precompute = 1
{extra}
"#
    )
}

fn load(collections: &[CollectionConfig]) -> super::LoadResult {
    super::load_collections(
        &ds_render::StyleContext::with_builtins(),
        collections,
        &[],
        "http://x",
        false,
        0,
        super::ReusableCaches::default(),
        super::EngineReuse::default(),
    )
}

fn health<'a>(result: &'a super::LoadResult, id: &str) -> &'a super::CollectionHealth {
    result
        .health
        .iter()
        .find(|h| h.id == id)
        .unwrap_or_else(|| panic!("a health entry"))
}

fn grid_engine(
    result: &super::LoadResult,
    id: &str,
) -> std::sync::Arc<engine_station_grid::StationGridEngine> {
    result
        .station_grid_engines
        .iter()
        .find(|e| e.collection_id() == id)
        .unwrap_or_else(|| panic!("a station grid engine"))
        .clone()
}

fn temperature_at(result: &super::LoadResult, id: &str, lon: f64, lat: f64) -> Option<f64> {
    let engine = &result.edr_state.engines[id];
    match engine
        .query_position(&format!("POINT({lon} {lat})"), None, None, None, None)
        .unwrap()
    {
        ds_core::model::CoverageResponse::Single(r) => r.ranges["temperature"].values[0],
        ds_core::model::CoverageResponse::Collection(_) => panic!("one coverage"),
    }
}

/// A CSV source, a GeoTIFF terrain and a GeoTIFF mask wired by id: the
/// grid waits for its poll to sample them, then serves corrected values on
/// land and nothing at sea, in EDR and in the map alike.
#[test]
fn station_grid_corrects_with_terrain_and_mask_collections() {
    let configs = collections(&format!(
        "{WEATHER}{TERRAIN}{LAND}{}{}",
        grid(
            "grid",
            "weather",
            r#"[{ name = "temperature", lapse_rate = -6.5 }]"#,
            "elevation_source = \"terrain\"\nland_mask_source = \"land\"",
        ),
        grid("plain", "weather", r#"["temperature"]"#, ""),
    ));
    let result = load(&configs);
    let h = health(&result, "grid");
    assert_eq!(
        h.status,
        super::CollectionStatus::Degraded,
        "unexpected value"
    );
    assert!(
        h.error
            .as_deref()
            .unwrap_or("")
            .contains("waiting for elevation_source 'terrain' and land_mask_source 'land'"),
        "unexpected value"
    );
    // The layers are styled at load, before the first analysis time.
    assert!(result.wms_state.styles.contains_key("grid/temperature"));
    assert!(result.edr_state.collections["grid"]
        .description
        .contains("the terrain of 'terrain'"));

    let engine = grid_engine(&result, "grid");
    assert!(!engine.has_data());
    engine.poll_once();
    assert!(engine.has_data(), "unexpected value");
    grid_engine(&result, "plain").poll_once();

    // At sea (the Bothnian Sea off Pori): nothing; the plain grid has a value.
    let (sea_lon, sea_lat) = (21.025, 61.475);
    assert_eq!(temperature_at(&result, "grid", sea_lon, sea_lat), None);
    assert!(temperature_at(&result, "plain", sea_lon, sea_lat).is_some());
    // On land: corrected for the cell's height against the stations'.
    let (lon, lat) = (22.775, 61.025);
    let corrected = temperature_at(&result, "grid", lon, lat).unwrap();
    let plain = temperature_at(&result, "plain", lon, lat).unwrap();
    assert!(
        corrected != plain && (corrected - plain).abs() < 3.0,
        "the correction moves the land value by under 3 °C"
    );

    // The map draws what EDR answers, one pixel per cell.
    let map = &result.wms_state.engines["grid"];
    let tile = map
        .get_raster_tile(
            [21.0, 60.0, 23.0, 62.0],
            40,
            40,
            None,
            &ds_core::map_engine::OutputCrs::Wgs84,
            Some("temperature"),
            None,
            None,
        )
        .unwrap();
    let pixel = |lon: f64, lat: f64| {
        let (col, row) = (
            ((lon - 21.0) / 0.05) as usize,
            ((62.0 - lat) / 0.05) as usize,
        );
        tile.values.value_at(row * 40 + col)
    };
    assert_eq!(pixel(sea_lon, sea_lat), None);
    assert!((pixel(lon, lat).unwrap() - corrected).abs() < 1e-4);
}

/// The terrain and the mask must be non-derived raster collections of the
/// same config; anything else fails the grid alone.
#[test]
fn station_grid_terrain_and_mask_must_be_raster_collections() {
    for (extra, needle) in [
        (
            "elevation_source = \"no-such\"",
            "elevation_source 'no-such' not found",
        ),
        (
            "elevation_source = \"weather\"",
            "elevation_source 'weather' is a csv collection, not a raster map collection",
        ),
        (
            "land_mask_source = \"plain\"",
            "land_mask_source 'plain' is a derived station-grid collection",
        ),
    ] {
        let configs = collections(&format!(
            "{WEATHER}{}{}",
            grid("plain", "weather", r#"["temperature"]"#, ""),
            grid(
                "grid",
                "weather",
                r#"[{ name = "temperature", lapse_rate = -6.5 }]"#,
                extra
            ),
        ));
        let result = load(&configs);
        let h = health(&result, "grid");
        assert_eq!(
            h.status,
            super::CollectionStatus::Failed,
            "unexpected value"
        );
        assert!(
            h.error.as_deref().unwrap_or("").contains(needle),
            "unexpected value"
        );
        assert!(!result.wms_state.engines.contains_key("grid"));
        assert_eq!(
            health(&result, "plain").status,
            super::CollectionStatus::Ready
        );
    }
}

/// A grid keeps its engine across a reload only when its terrain and mask
/// keep theirs, as with its source (a BUFR collection here: a CSV source
/// is rebuilt on every reload, and its grid with it).
#[test]
fn station_grid_reuse_follows_its_terrain_and_mask() {
    let body = |terrain_poll: u64| {
        format!(
            "{SYNOP}{}\npoll_interval_secs = {terrain_poll}\n{LAND}{}",
            TERRAIN.trim_end(),
            grid(
                "grid",
                "synop",
                r#"["air_temperature"]"#,
                "elevation_source = \"terrain\"\nland_mask_source = \"land\"",
            ),
        )
    };
    let old_configs = collections(&body(30));
    let built = load(&old_configs);
    assert!(built.engines_by_id.contains_key("grid"), "unexpected value");
    let old: HashMap<String, CollectionConfig> = old_configs
        .iter()
        .map(|c| (c.id.clone(), c.clone()))
        .collect();
    let live = built.engines_by_id.clone();
    let reused = super::reusable_collections(&old, &old_configs, &live);
    assert!(
        ["synop", "terrain", "land", "grid"]
            .iter()
            .all(|id| reused.contains(*id)),
        "unexpected value"
    );
    // A changed terrain is rebuilt, so is the grid that reads it.
    let changed = collections(&body(60));
    let reused = super::reusable_collections(&old, &changed, &live);
    assert!(
        !reused.contains("terrain") && !reused.contains("grid"),
        "unexpected value"
    );
    assert!(reused.contains("land") && reused.contains("synop"));
}
