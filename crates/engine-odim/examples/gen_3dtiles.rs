//! Phase-1 PoC: turn a polar volume into an OGC 3D Tiles point cloud that
//! loads in stock CesiumJS. Every valid echo cell becomes a point at its true
//! 3D position, computed with the engine's 4/3-Earth beam model, and colored by
//! an NWS-style reflectivity ramp.
//!
//! Content is the `.pnts` (Point Cloud) tile format. It is the 3D Tiles 1.0
//! point-cloud format (still rendered by CesiumJS in 1.1) and the path where
//! `pointSize`/attenuation styling and per-point RGB actually work — a `.glb`
//! with POINTS mode goes through the Model renderer, which draws fixed 1px
//! white points and ignores both the style and vertex colors. A production
//! feature would move to glTF voxels (EXT_primitive_voxels, cylinder shape);
//! pnts is the right call for a "see it now" geometry PoC. Either way this
//! exercises the hard part: (slant_range, elevation, azimuth) -> geographic ->
//! ECEF, via an RTC center.
//!
//! Usage:
//!   cargo run -p engine-odim --example gen_3dtiles -- <file.h5> [out_dir] [min_dbz]
//!     out_dir  default target/3dtiles-fivih
//!     min_dbz  default 5.0  (skip cells below this reflectivity)

use engine_odim::pvol::read_moment_pixels;
use engine_odim::pvol::read_polar_volume;
use std::f64::consts::PI;
use std::fs;
use std::path::PathBuf;

// WGS84 ellipsoid.
const WGS84_A: f64 = 6_378_137.0;
const WGS84_E2: f64 = 6.694_379_990_14e-3;
// 4/3-Earth effective radius for beam propagation (matches volume_engine.rs).
const FOUR_THIRDS_EARTH_M: f64 = 4.0 / 3.0 * 6_371_000.0;
// Sphere radius for the great-circle "destination point" along the ground.
const EARTH_SPHERE_M: f64 = 6_371_000.0;

/// (lon_deg, lat_deg, height_m above ellipsoid) -> ECEF (x, y, z) metres.
fn geodetic_to_ecef(lon_deg: f64, lat_deg: f64, h: f64) -> [f64; 3] {
    let lon = lon_deg.to_radians();
    let lat = lat_deg.to_radians();
    let (sl, cl) = lat.sin_cos();
    let (so, co) = lon.sin_cos();
    let n = WGS84_A / (1.0 - WGS84_E2 * sl * sl).sqrt();
    [
        (n + h) * cl * co,
        (n + h) * cl * so,
        (n * (1.0 - WGS84_E2) + h) * sl,
    ]
}

/// Great-circle destination from (lon0,lat0) travelling `dist` m along
/// `bearing` (deg clockwise from north). Spherical — matches the engine.
fn destination_point(lon0: f64, lat0: f64, dist: f64, bearing_deg: f64) -> (f64, f64) {
    let ad = dist / EARTH_SPHERE_M;
    let br = bearing_deg.to_radians();
    let lat1 = lat0.to_radians();
    let lon1 = lon0.to_radians();
    let lat2 = (lat1.sin() * ad.cos() + lat1.cos() * ad.sin() * br.cos()).asin();
    let lon2 = lon1 + (br.sin() * ad.sin() * lat1.cos()).atan2(ad.cos() - lat1.sin() * lat2.sin());
    let mut lon = lon2.to_degrees();
    lon = (lon + 540.0) % 360.0 - 180.0; // normalise to [-180, 180)
    (lon, lat2.to_degrees())
}

/// 4/3-Earth beam model: slant range + elevation angle -> (ground arc
/// distance, height above antenna). Same formula as volume_engine.rs.
fn slant_to_ground_height(r: f64, elangle_deg: f64) -> (f64, f64) {
    let el = elangle_deg.to_radians();
    let rp = FOUR_THIRDS_EARTH_M;
    let h = (r * r + rp * rp + 2.0 * r * rp * el.sin()).sqrt() - rp;
    let s = rp * (r * el.cos() / (r * el.sin() + rp)).atan();
    (s, h)
}

/// NWS-style reflectivity colour ramp. dBZ -> (r,g,b).
fn dbz_color(dbz: f64) -> [u8; 3] {
    const STOPS: &[(f64, [u8; 3])] = &[
        (5.0, [4, 233, 231]),
        (10.0, [1, 159, 244]),
        (15.0, [3, 0, 244]),
        (20.0, [2, 253, 2]),
        (25.0, [1, 197, 1]),
        (30.0, [0, 142, 0]),
        (35.0, [253, 248, 2]),
        (40.0, [229, 188, 0]),
        (45.0, [253, 149, 0]),
        (50.0, [253, 0, 0]),
        (55.0, [212, 0, 0]),
        (60.0, [188, 0, 0]),
        (65.0, [248, 0, 253]),
        (70.0, [152, 84, 198]),
    ];
    if dbz <= STOPS[0].0 {
        return STOPS[0].1;
    }
    let last = STOPS.len() - 1;
    if dbz >= STOPS[last].0 {
        return STOPS[last].1;
    }
    for w in STOPS.windows(2) {
        let (lo, c0) = w[0];
        let (hi, c1) = w[1];
        if dbz >= lo && dbz <= hi {
            let t = (dbz - lo) / (hi - lo);
            return [
                (c0[0] as f64 + t * (c1[0] as f64 - c0[0] as f64)).round() as u8,
                (c0[1] as f64 + t * (c1[1] as f64 - c0[1] as f64)).round() as u8,
                (c0[2] as f64 + t * (c1[2] as f64 - c0[2] as f64)).round() as u8,
            ];
        }
    }
    STOPS[last].1
}

fn main() {
    let mut args = std::env::args().skip(1);
    // Default to the FMI Vihti polar volume the integration tests use. Like
    // those tests, this 15 MB fixture is NOT committed to git (see the demo
    // README) — pass any ODIM polar volume (.h5) as the first arg instead.
    let path = args.next().unwrap_or_else(|| {
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

    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(err) => {
            eprintln!("cannot read {path}: {err}");
            eprintln!(
                "The default FMI Vihti PVOL fixture is not committed to git (15 MB). \
                 Pass a path to an ODIM polar volume (.h5), or place one under \
                 testdata/radar-fmi-pvol/."
            );
            std::process::exit(1);
        }
    };
    let vol = read_polar_volume(&bytes).expect("parse volume");
    let site = &vol.site;
    // Non-finite antenna coordinates would poison both the ECEF projection
    // (NaN positions) and the generated viewer (`f64::INFINITY` formats as the
    // invalid-JS literal `inf`). Refuse a corrupt volume up front.
    if !(site.lon.is_finite() && site.lat.is_finite() && site.height.is_finite()) {
        eprintln!(
            "non-finite antenna coordinates (lon={}, lat={}, h={}) — refusing to encode",
            site.lon, site.lat, site.height
        );
        std::process::exit(1);
    }
    eprintln!(
        "site {:?} ({:?})  lon={:.4} lat={:.4} h={:.0}m  sweeps={}  time={}",
        site.nod,
        site.plc,
        site.lon,
        site.lat,
        site.height,
        vol.sweeps.len(),
        vol.time
    );

    // RTC center = ECEF of the antenna. pnts POSITION values are raw ECEF
    // offsets from this center (no Y-up/Z-up flip — pnts is ECEF-native).
    let center = geodetic_to_ecef(site.lon, site.lat, site.height);

    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut colors: Vec<[u8; 3]> = Vec::new();
    // Geodetic bbox for the `region` bounding volume.
    let (mut w, mut s, mut e, mut n) = (PI, PI / 2.0, -PI, -PI / 2.0);
    let (mut min_h, mut max_h) = (f64::INFINITY, f64::NEG_INFINITY);

    for sw in &vol.sweeps {
        let Some(mom) = sw
            .moments
            .iter()
            .find(|m| m.quantity == "DBZH")
            .or_else(|| sw.moments.iter().find(|m| m.quantity == "TH"))
        else {
            continue;
        };
        let px = match read_moment_pixels(&bytes, &mom.dataset_path, sw.nrays, sw.nbins) {
            Ok(p) => p,
            Err(err) => {
                eprintln!("  skip sweep {:.2}: {err:?}", sw.elangle);
                continue;
            }
        };
        let deg_per_ray = 360.0 / sw.nrays as f64;
        for ray in 0..sw.nrays {
            let bearing = (ray as f64 + 0.5) * deg_per_ray;
            for bin in 0..sw.nbins {
                let Some(dbz) = px.sample(
                    ray,
                    bin,
                    mom.gain,
                    mom.offset,
                    mom.nodata,
                    Some(mom.undetect),
                ) else {
                    continue;
                };
                if dbz < min_dbz {
                    continue;
                }
                let r = sw.rstart + (bin as f64 + 0.5) * sw.rscale;
                let (ground, h_above) = slant_to_ground_height(r, sw.elangle);
                let (lon, lat) = destination_point(site.lon, site.lat, ground, bearing);
                let alt = site.height + h_above;
                let ecef = geodetic_to_ecef(lon, lat, alt);
                positions.push([
                    (ecef[0] - center[0]) as f32,
                    (ecef[1] - center[1]) as f32,
                    (ecef[2] - center[2]) as f32,
                ]);
                colors.push(dbz_color(dbz));

                let lonr = lon.to_radians();
                let latr = lat.to_radians();
                w = w.min(lonr);
                e = e.max(lonr);
                s = s.min(latr);
                n = n.max(latr);
                min_h = min_h.min(alt);
                max_h = max_h.max(alt);
            }
        }
    }

    let count = positions.len();
    if count == 0 {
        eprintln!("no cells >= {min_dbz} dBZ — nothing to write");
        std::process::exit(1);
    }
    eprintln!("points: {count}  (>= {min_dbz} dBZ)");

    fs::create_dir_all(&out_dir).expect("mkdir out_dir");

    // ---- content.pnts ----
    // Binary body: POSITION (n*12, f32) then RGB (n*3, u8).
    let pos_bytes = count * 12;
    let rgb_off = pos_bytes;
    let mut body = Vec::with_capacity(pos_bytes + count * 3);
    for p in &positions {
        for v in p {
            body.extend_from_slice(&v.to_le_bytes());
        }
    }
    for c in &colors {
        body.extend_from_slice(c);
    }
    while body.len() % 8 != 0 {
        body.push(0);
    }

    let [cx, cy, cz] = center;
    let ft_json = format!(
        r#"{{"POINTS_LENGTH":{count},"RTC_CENTER":[{cx},{cy},{cz}],"POSITION":{{"byteOffset":0}},"RGB":{{"byteOffset":{rgb_off}}}}}"#
    );
    let mut ft_json = ft_json.into_bytes();
    while ft_json.len() % 8 != 0 {
        ft_json.push(b' ');
    }

    let header_len = 28;
    let total = header_len + ft_json.len() + body.len();
    // The pnts header fields are u32: fail loudly rather than silently
    // truncate (a >4 GB tile would otherwise produce a corrupt file).
    let total_u32 = u32::try_from(total).expect("pnts file exceeds 4 GB");
    let ft_json_u32 = u32::try_from(ft_json.len()).expect("pnts feature-table JSON exceeds 4 GB");
    let body_u32 = u32::try_from(body.len()).expect("pnts feature-table binary exceeds 4 GB");
    let mut pnts = Vec::with_capacity(total);
    pnts.extend_from_slice(b"pnts");
    pnts.extend_from_slice(&1u32.to_le_bytes()); // version
    pnts.extend_from_slice(&total_u32.to_le_bytes());
    pnts.extend_from_slice(&ft_json_u32.to_le_bytes()); // FT JSON len
    pnts.extend_from_slice(&body_u32.to_le_bytes()); // FT binary len
    pnts.extend_from_slice(&0u32.to_le_bytes()); // batch table JSON len
    pnts.extend_from_slice(&0u32.to_le_bytes()); // batch table binary len
    pnts.extend_from_slice(&ft_json);
    pnts.extend_from_slice(&body);
    fs::write(out_dir.join("content.pnts"), &pnts).expect("write pnts");

    // ---- tileset.json ----
    // Region bounding volume is geodetic (EPSG:4979); RTC_CENTER inside the
    // pnts places points in ECEF, so no tile transform is needed.
    let region = format!("[{w},{s},{e},{n},{min_h},{max_h}]");
    // A non-zero top-level geometricError is load-bearing: with the tileset
    // geometricError at 0, CesiumJS never refines to the root and the content
    // tile is never even requested (it draws nothing). Give the tileset a
    // large error and the single content tile a positive leaf error.
    let tileset = format!(
        r#"{{
  "asset": {{ "version": "1.1" }},
  "geometricError": 100000,
  "root": {{
    "boundingVolume": {{ "region": {region} }},
    "geometricError": 1000,
    "refine": "ADD",
    "content": {{ "uri": "content.pnts" }}
  }}
}}
"#
    );
    fs::write(out_dir.join("tileset.json"), tileset).expect("write tileset");

    // Self-describing HUD/title from the actual volume, not hardcoded.
    let site_label = match (&site.plc, &site.nod) {
        (Some(plc), Some(nod)) => format!("{plc} ({nod})"),
        (Some(name), None) | (None, Some(name)) => name.clone(),
        (None, None) => "radar".to_string(),
    };
    let hud = format!(
        "{site_label} polar volume — {} · DBZH ≥ {min_dbz:.0} dBZ",
        vol.time.format("%Y-%m-%d %H:%MZ")
    );
    write_viewer(&out_dir, site.lon, site.lat, site.height, &hud);

    eprintln!(
        "wrote {}/  (tileset.json, content.pnts, index.html)",
        out_dir.display()
    );
    eprintln!(
        "pnts {:.1} MB  region lon[{:.3},{:.3}] lat[{:.3},{:.3}] alt[{:.0},{:.0}]m",
        pnts.len() as f64 / 1e6,
        w.to_degrees(),
        e.to_degrees(),
        s.to_degrees(),
        n.to_degrees(),
        min_h,
        max_h
    );
}

/// Write a self-contained CesiumJS viewer. `hud` describes the actual volume
/// (site/time/threshold) so the page is correct for any input, not just fivih.
fn write_viewer(out_dir: &std::path::Path, lon: f64, lat: f64, h: f64, hud: &str) {
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
    // Camera sits ~90 km above the antenna's own elevation, south of it, so the
    // framing tracks the site rather than assuming sea level.
    let cam_alt = h + 90_000.0;
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
    destination: Cesium.Cartesian3.fromDegrees({lon}, {lat_s}, {cam_alt}),
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
