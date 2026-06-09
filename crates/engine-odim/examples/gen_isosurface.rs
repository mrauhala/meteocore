//! Isosurface demo (#357): build a `PolarVolumeEngine` over a directory of ODIM
//! polar volumes, resample a site into a cylindrical `ds_core::volume::VoxelGrid`
//! via `VolumeEngine::read_voxel_grid`, extract a reflectivity **isosurface**
//! (marching tetrahedra) at a chosen threshold, and encode it to a glTF `.glb`
//! triangle mesh + `tileset.json` with `ds-3dtiles`.
//!
//! Unlike the `.pnts` point-cloud demo (`gen_3dtiles`), the output is a plain
//! glTF mesh — a solid "storm shell" at e.g. 20 dBZ — that renders in any 3D
//! Tiles 1.1 client (no experimental voxel extension). The marching-tetrahedra
//! mesher and the cylindrical-index → ECEF geometry live in `ds-3dtiles` /
//! `ds-core`; this example only wires them together and writes a token-free
//! CesiumJS viewer.
//!
//! Usage:
//!   cargo run -p engine-odim --example gen_isosurface -- [file.h5] [out_dir] [threshold_dbz]
//!     file.h5        default: the (uncommitted) FMI Vihti fixture
//!     out_dir        default: target/3dtiles-iso-fivih
//!     threshold_dbz  default: 20.0  (the reflectivity shell to draw)

use ds_core::config::OdimConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::geo::geodetic_to_ecef;
use ds_core::volume::VolumeEngine;
use ds_render::{BuiltinColormap, ColorMap, LutColorMap};
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
            .unwrap_or_else(|| "target/3dtiles-iso-fivih".to_string()),
    );
    let threshold: f64 = args
        .next()
        .map(|s| s.parse().expect("threshold_dbz must be a number"))
        .unwrap_or(20.0);

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
    let engine = PolarVolumeEngine::new("iso-demo", Some(data_dir), &config)
        .expect("build PolarVolumeEngine over the directory");

    let sites = engine.sites();
    let Some((nod, _label)) = sites.first() else {
        eprintln!("no radar sites found in {data_dir}");
        std::process::exit(1);
    };
    let view = engine.site_view(nod, &format!("iso-demo-{nod}"));

    let info = view.volume_info();
    let quantity = ["DBZH", "TH"]
        .into_iter()
        .find(|q| info.quantities.iter().any(|(id, _)| id == q))
        .map(str::to_string)
        .or_else(|| info.quantities.first().map(|(id, _)| id.clone()))
        .expect("site advertises at least one quantity");

    // Resample the volume into the default cylindrical voxel grid, then mesh the
    // isosurface — the production code path the future voxel/iso route will use.
    let grid = view
        .read_voxel_grid(Some(&quantity), None, None, None)
        .expect("resample the volume into a voxel grid");
    eprintln!(
        "site {nod} — voxel grid {:?}, {} finite cells, quantity {}",
        grid.dims,
        grid.valid_count(),
        grid.quantity
    );

    // The shell's colour = the colormap at the threshold value.
    let colormap = LutColorMap::from_builtin(BuiltinColormap::RadarDbz, -32.0, 95.0);
    let color = colormap.color(Some(threshold));
    // Seal at the shared no-echo floor (the engine fills clear air with it too,
    // #360) so the shell closes into solid blobs (the preferred look);
    // background=Some additionally seals the genuinely-unmeasured (NaN) cells, so
    // there are no open boundaries. (Pass None instead for honest open
    // boundaries — leaves the cone of silence / below-lowest-beam uncapped.)
    let background = Some(f64::from(ds_core::volume::NO_ECHO_FLOOR_DBZ));
    let glb = ds_3dtiles::encode_isosurface_glb(&grid, threshold, color, background)
        .expect("mesh the isosurface (try a lower threshold if this reports 'empty')");

    fs::create_dir_all(&out_dir).expect("mkdir out_dir");
    fs::write(out_dir.join("content.glb"), &glb).expect("write content.glb");

    // Prefer the engine's authoritative coverage region (the same value the
    // production `/tileset.json` endpoint uses), falling back to an
    // origin±angular-radius approximation only if it's absent. lon/lat in
    // radians (3D Tiles `region` layout).
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
        "{site_label} ({nod}) {} isosurface — {time_str} · {threshold:.0} dBZ shell",
        grid.quantity
    );
    write_viewer(&out_dir, &hud);

    eprintln!(
        "wrote {}/  (tileset.json, content.glb, index.html) — {:.1} MB glb",
        out_dir.display(),
        glb.len() as f64 / 1e6
    );
}

/// Write a self-contained, token-free CesiumJS viewer (CARTO Dark Matter
/// basemap) for the glTF isosurface tileset. The camera auto-frames the tileset
/// (`zoomTo`) so it works regardless of where the echoes sit — no hard-coded
/// position to land on empty sky.
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
// Token-free: CARTO Dark Matter basemap, no Cesium Ion terrain/imagery.
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
// Lift the dark basemap a touch so the shell sits on a visible map.
viewer.imageryLayers.get(0).brightness = 1.6;
// Light the mesh from the scene's sun; the doubleSided material shades both
// faces so the shell reads as a solid 3-D surface.
viewer.scene.light = new Cesium.DirectionalLight({{
  direction: Cesium.Cartesian3.normalize(new Cesium.Cartesian3(0.3, -0.6, -0.7), new Cesium.Cartesian3()),
}});
Cesium.Cesium3DTileset.fromUrl("tileset.json", {{ maximumScreenSpaceError: 1 }}).then(ts => {{
  viewer.scene.primitives.add(ts);
  // Auto-frame the tileset obliquely (heading 20°, pitch -25°) from its own
  // bounding sphere — robust whatever the echo layout.
  viewer.zoomTo(ts, new Cesium.HeadingPitchRange(
    Cesium.Math.toRadians(20), Cesium.Math.toRadians(-25), ts.boundingSphere.radius * 1.3));
  console.log("isosurface tileset ready");
}}).catch(e => console.error("tileset error", e));
</script></body></html>
"#
    );
    fs::write(out_dir.join("index.html"), html).expect("write index.html");
}
