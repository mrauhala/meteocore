//! Echo-Top-Height demo (#362): build a `PolarVolumeEngine` over a directory of
//! ODIM polar volumes, resample a site into a cylindrical
//! `ds_core::volume::VoxelGrid`, and encode the **echo-top-height draped
//! surface** to a glTF `.glb` + `tileset.json` with `ds-3dtiles`.
//!
//! Unlike the isosurface (`gen_isosurface`, a closed shell at one dBZ value),
//! this collapses the volume to a 2-D height field — the highest altitude where
//! reflectivity ≥ a threshold, per ground column — draped as one surface
//! coloured by height (low = blue → high = red). It shows storm *depth*.
//!
//! Usage:
//!   cargo run -p engine-odim --example gen_echo_top -- [file.h5] [out_dir] [threshold_dbz] [n_radius] [n_height]
//!     file.h5        default: the (uncommitted) FMI Vihti fixture
//!     out_dir        default: target/3dtiles-echotop-fivih
//!     threshold_dbz  default: 18.0  (the standard echo-top reflectivity)
//!     n_radius       default: 128   (radial cells ≈ 2 km bins; ~500 ≈ native)
//!     n_height       default: 48    (height cells ≈ 420 m; capped by ~10 sweeps)

use ds_core::config::OdimConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::geo::geodetic_to_ecef;
use ds_core::volume::VolumeEngine;
use ds_render::{ColorStop, LutColorMap};
use engine_odim::PolarVolumeEngine;
use std::fs;
use std::path::{Path, PathBuf};

/// Height range (metres) the colour ramp spans — typical echo tops 0–15 km.
const HEIGHT_MIN_M: f64 = 0.0;
const HEIGHT_MAX_M: f64 = 15_000.0;

fn main() {
    let mut args = std::env::args().skip(1);
    let file = args.next().unwrap_or_else(|| {
        format!(
            "{}/../../testdata/radar-fmi-pvol/202605191050_fivih_PVOL.h5",
            env!("CARGO_MANIFEST_DIR")
        )
    });
    let out_dir = PathBuf::from(
        args.next()
            .unwrap_or_else(|| "target/3dtiles-echotop-fivih".to_string()),
    );
    let threshold: f64 = args
        .next()
        .map(|s| s.parse().expect("threshold_dbz must be a number"))
        .unwrap_or(18.0);
    // Optional resampling resolution: radial cells (default 128 ≈ 2 km bins) and
    // height cells (default 48 ≈ 420 m). Azimuth stays 360 = 1° (native). The
    // fivih range bins are 125–500 m natively, so bumping radial toward ~500
    // sharpens the bars; bumping azimuth past 360 or height past the ~10 sweeps
    // adds no real detail. Bounded by the engine's MAX_VOXELS.
    let n_radius: usize = args
        .next()
        .map(|s| s.parse().expect("n_radius must be an integer"))
        .unwrap_or(128);
    let n_height: usize = args
        .next()
        .map(|s| s.parse().expect("n_height must be an integer"))
        .unwrap_or(48);

    let file = Path::new(&file);
    if !file.exists() {
        eprintln!("cannot find {}", file.display());
        eprintln!(
            "The default FMI Vihti PVOL fixture is not committed to git (15 MB). \
             Pass a path to an ODIM polar volume (.h5), or place one under \
             testdata/radar-fmi-pvol/."
        );
        std::process::exit(1);
    }
    let data_dir = file
        .parent()
        .and_then(Path::to_str)
        .expect("file has a UTF-8 parent directory");

    let config = OdimConfig {
        filename_template: None,
        filename_pattern: None,
        timestamp_format: None,
        parameter: None,
        unit: None,
        nodata: None,
        gain: None,
        offset: None,
        poll_interval_secs: 30,
        max_files: None,
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        discovery: None,
        cadence_secs: None,
    };
    let engine = PolarVolumeEngine::new("echotop-demo", Some(data_dir), &config)
        .expect("build PolarVolumeEngine over the directory");

    let sites = engine.sites();
    let Some((nod, _label)) = sites.first() else {
        eprintln!("no radar sites found in {data_dir}");
        std::process::exit(1);
    };
    let view = engine.site_view(nod, &format!("echotop-demo-{nod}"));

    let info = view.volume_info();
    let quantity = ["DBZH", "TH"]
        .into_iter()
        .find(|q| info.quantities.iter().any(|(id, _)| id == q))
        .map(str::to_string)
        .or_else(|| info.quantities.first().map(|(id, _)| id.clone()))
        .expect("site advertises at least one quantity");

    let grid = view
        .read_voxel_grid(Some(&quantity), None, Some([n_radius, 360, n_height]), None)
        .expect("resample the volume into a voxel grid");
    eprintln!(
        "site {nod} — voxel grid {:?} ({} cells), quantity {}",
        grid.dims,
        grid.dims[0] * grid.dims[1] * grid.dims[2],
        grid.quantity
    );

    // Colour the draped surface by HEIGHT (not reflectivity): blue (shallow) →
    // red (deep). Stops are at **height values** — a builtin colormap's stops
    // are in its own units (e.g. Temperature's are °C), so they collapse when
    // stretched over a 0–15 km range. Build the ramp explicitly.
    let stops = [
        (0.0_f64, [40u8, 70, 200, 255]), // deep blue — shallow
        (3000.0, [0, 200, 220, 255]),    // cyan
        (6000.0, [40, 200, 80, 255]),    // green
        (9000.0, [240, 230, 60, 255]),   // yellow
        (12000.0, [240, 140, 40, 255]),  // orange
        (15000.0, [220, 40, 40, 255]),   // red — deep
    ]
    .map(|(value, color)| ColorStop { value, color });
    let height_map = LutColorMap::from_stops(&stops, HEIGHT_MIN_M, HEIGHT_MAX_M);
    // Extruded columns (one box per bin, ground → echo top) — solid walls,
    // grounded, blocky-by-bin. (`encode_echo_top_glb` is the thin draped-surface
    // variant.)
    let glb = ds_3dtiles::encode_echo_top_columns_glb(&grid, threshold, &height_map)
        .expect("mesh the echo-top columns (try a lower threshold if this reports 'empty')");

    fs::create_dir_all(&out_dir).expect("mkdir out_dir");
    fs::write(out_dir.join("content.glb"), &glb).expect("write content.glb");

    // Region (geodetic, radians) + antenna ECEF transform — same placement as
    // the isosurface (the engine's coverage region, or an origin±radius box).
    let region = info.region.unwrap_or_else(|| {
        let earth_r = ds_core::geo::EARTH_RADIUS_M;
        let radius_max = grid.radius_range[1];
        let lat_r = grid.origin_lat.to_radians();
        let dlat = radius_max / earth_r;
        let dlon = radius_max / (earth_r * lat_r.cos().max(1e-6));
        let lon_r = grid.origin_lon.to_radians();
        [
            lon_r - dlon,
            lat_r - dlat,
            lon_r + dlon,
            lat_r + dlat,
            grid.origin_height + grid.height_range[0],
            grid.origin_height + grid.height_range[1],
        ]
    });
    let rtc = geodetic_to_ecef(grid.origin_lon, grid.origin_lat, grid.origin_height);
    let tileset = ds_3dtiles::tileset_json_glb(region, "content.glb", rtc).expect("build tileset");
    fs::write(out_dir.join("tileset.json"), tileset).expect("write tileset.json");

    let antenna = view
        .get_locations()
        .ok()
        .and_then(|locs| locs.into_iter().next());
    let site_label = antenna
        .as_ref()
        .map(|l| l.label.clone())
        .unwrap_or_else(|| nod.clone());
    let time_str = info
        .times
        .last()
        .map(|t| t.format("%Y-%m-%d %H:%MZ").to_string())
        .unwrap_or_default();
    let hud = format!(
        "{site_label} ({nod}) echo-top height — {time_str} · {threshold:.0} dBZ · coloured 0–15 km",
    );
    write_viewer(&out_dir, &hud);

    eprintln!(
        "wrote {}/  (tileset.json, content.glb, index.html) — {:.1} MB glb",
        out_dir.display(),
        glb.len() as f64 / 1e6
    );
}

/// Token-free CesiumJS viewer (CARTO Dark Matter), auto-framing the tileset.
fn write_viewer(out_dir: &Path, hud: &str) {
    fn html_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
    let hud = html_escape(hud);
    let html = format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>{hud} — 3D Tiles</title>
<script src="https://cesium.com/downloads/cesiumjs/releases/1.124/Build/Cesium/Cesium.js"></script>
<link href="https://cesium.com/downloads/cesiumjs/releases/1.124/Build/Cesium/Widgets/widgets.css" rel="stylesheet">
<style>html,body,#c{{width:100%;height:100%;margin:0;overflow:hidden}}
#hud{{position:absolute;top:8px;left:8px;z-index:9;font:13px sans-serif;color:#fff;background:rgba(0,0,0,.55);padding:6px 9px;border-radius:5px}}</style></head>
<body><div id="c"></div><div id="hud">{hud}</div><script>
const viewer = new Cesium.Viewer("c", {{
  baseLayer: new Cesium.ImageryLayer(
    new Cesium.UrlTemplateImageryProvider({{
      url: "https://{{s}}.basemaps.cartocdn.com/dark_all/{{z}}/{{x}}/{{y}}.png",
      subdomains: "abcd", maximumLevel: 19,
      credit: "© OpenStreetMap contributors © CARTO",
    }})),
  baseLayerPicker: false, timeline: false, animation: false, geocoder: false,
}});
viewer.scene.backgroundColor = Cesium.Color.BLACK;
viewer.scene.globe.depthTestAgainstTerrain = false;
viewer.imageryLayers.get(0).brightness = 1.6;
viewer.scene.light = new Cesium.DirectionalLight({{
  direction: Cesium.Cartesian3.normalize(new Cesium.Cartesian3(0.3, -0.6, -0.7), new Cesium.Cartesian3()),
}});
Cesium.Cesium3DTileset.fromUrl("tileset.json", {{ maximumScreenSpaceError: 1 }}).then(ts => {{
  viewer.scene.primitives.add(ts);
  viewer.zoomTo(ts, new Cesium.HeadingPitchRange(
    Cesium.Math.toRadians(20), Cesium.Math.toRadians(-30), ts.boundingSphere.radius * 1.3));
  console.log("echo-top tileset ready");
}}).catch(e => console.error("tileset error", e));
</script></body></html>
"#
    );
    fs::write(out_dir.join("index.html"), html).expect("write index.html");
}
