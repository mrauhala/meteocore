//! Phase-1 demo, now a **thin driver** over the real stack (#348): build a
//! `PolarVolumeEngine` over a directory of ODIM polar volumes, sample a site
//! into a `ds_core::volume::VolumePointCloud` via `VolumeEngine`, and encode it
//! to an OGC 3D Tiles `.pnts` tile + `tileset.json` with `ds-3dtiles`. The
//! geometry (4/3-Earth beam model → ECEF) and the `.pnts`/tileset encoding now
//! live in the engine and `ds-3dtiles`; this example only wires them together
//! and writes a token-free CesiumJS viewer.
//!
//! Usage:
//!   cargo run -p engine-odim --example gen_3dtiles -- [file.h5] [out_dir] [min_dbz]
//!     file.h5  default: the (uncommitted) FMI Vihti fixture
//!     out_dir  default: target/3dtiles-fivih
//!     min_dbz  default: 5.0  (drop cells below this reflectivity)

use ds_core::config::OdimConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::volume::VolumeEngine;
use ds_render::{BuiltinColormap, LutColorMap};
use engine_odim::PolarVolumeEngine;
use std::fs;
use std::path::{Path, PathBuf};

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
            .unwrap_or_else(|| "target/3dtiles-fivih".to_string()),
    );
    let min_dbz: f64 = args
        .next()
        .map(|s| s.parse().expect("min_dbz must be a number"))
        .unwrap_or(5.0);

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

    // The PVOL engine scans a directory and expands it into one collection per
    // radar site. `odim-volume` ignores parameter/unit/overrides.
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
    let engine = PolarVolumeEngine::new("3dtiles-demo", Some(data_dir), &config)
        .expect("build PolarVolumeEngine over the directory");

    let sites = engine.sites();
    let Some((nod, _label)) = sites.first() else {
        eprintln!("no radar sites found in {data_dir}");
        std::process::exit(1);
    };
    let view = engine.site_view(nod, &format!("3dtiles-demo-{nod}"));

    // For the demo, render a reflectivity quantity (the engine's own default is
    // the first advertised quantity, which may be a classification field like
    // CSP). Prefer DBZH, then TH, else whatever the site advertises first.
    let info = view.volume_info();
    let quantity = ["DBZH", "TH"]
        .into_iter()
        .find(|q| info.quantities.iter().any(|(id, _)| id == q))
        .map(str::to_string)
        .or_else(|| info.quantities.first().map(|(id, _)| id.clone()))
        .expect("site advertises at least one quantity");

    // Sample the whole volume into a point cloud — the production code path.
    let cloud = view
        .read_point_cloud(Some(&quantity), None, Some(min_dbz), None)
        .expect("sample the volume into a point cloud");
    eprintln!(
        "site {nod} — {} points (>= {min_dbz} dBZ), quantity {}",
        cloud.points.len(),
        cloud.quantity
    );

    fs::create_dir_all(&out_dir).expect("mkdir out_dir");

    // Encode via ds-3dtiles (the same path the API layer will use, #349).
    let colormap = LutColorMap::from_builtin(BuiltinColormap::RadarDbz, -32.0, 95.0);
    let pnts = ds_3dtiles::encode_pnts(&cloud, &colormap).expect("encode pnts");
    fs::write(out_dir.join("content.pnts"), &pnts).expect("write content.pnts");
    let tileset = ds_3dtiles::tileset_json(&cloud, "content.pnts").expect("build tileset.json");
    fs::write(out_dir.join("tileset.json"), tileset).expect("write tileset.json");

    // Antenna position (for camera framing) + a self-describing HUD.
    let antenna = view
        .get_locations()
        .ok()
        .and_then(|locs| locs.into_iter().next());
    let (lon, lat) = antenna
        .as_ref()
        .map(|l| (l.longitude, l.latitude))
        .unwrap_or((0.0, 0.0));
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
        "{site_label} ({nod}) polar volume — {time_str} · {} ≥ {min_dbz:.0} dBZ",
        cloud.quantity
    );
    write_viewer(&out_dir, lon, lat, &hud);

    eprintln!(
        "wrote {}/  (tileset.json, content.pnts, index.html) — {:.1} MB pnts",
        out_dir.display(),
        pnts.len() as f64 / 1e6
    );
}

/// Write a self-contained, token-free CesiumJS viewer (CARTO Dark Matter
/// basemap). `hud`/`lon`/`lat` describe the actual volume.
fn write_viewer(out_dir: &Path, lon: f64, lat: f64, hud: &str) {
    // `hud` carries HDF5-sourced site metadata (PLC/NOD) verbatim; escape it
    // before it lands in the HTML <title>/<div> so a crafted .h5 can't inject
    // markup/script into the generated viewer.
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
// Token-free: CARTO Dark Matter basemap (dark backdrop makes the dBZ
// colours pop), no Cesium Ion terrain/imagery.
const viewer = new Cesium.Viewer("c", {{
  baseLayer: new Cesium.ImageryLayer(
    new Cesium.UrlTemplateImageryProvider({{
      url: "https://{{s}}.basemaps.cartocdn.com/dark_all/{{z}}/{{x}}/{{y}}.png",
      subdomains: "abcd",
      maximumLevel: 19,
      credit: "© OpenStreetMap contributors © CARTO",
    }})),
  baseLayerPicker: false, timeline: false, animation: false, geocoder: false,
}});
viewer.scene.backgroundColor = Cesium.Color.BLACK;
viewer.scene.globe.depthTestAgainstTerrain = false;
Cesium.Cesium3DTileset.fromUrl("tileset.json", {{ maximumScreenSpaceError: 1 }}).then(ts => {{
  viewer.scene.primitives.add(ts);
  ts.style = new Cesium.Cesium3DTileStyle({{ pointSize: 5.0 }});
  // Frame the volume from the SW, elevated, to reveal vertical structure.
  viewer.camera.flyTo({{
    destination: Cesium.Cartesian3.fromDegrees({lon}, {lat_s}, 90000),
    orientation: {{ heading: 0, pitch: Cesium.Math.toRadians(-30), roll: 0 }},
    duration: 0
  }});
  console.log("pnts tileset ready");
}}).catch(e => console.error("tileset error", e));
</script></body></html>
"#,
        lat_s = lat - 0.9,
    );
    fs::write(out_dir.join("index.html"), html).expect("write index.html");
}
