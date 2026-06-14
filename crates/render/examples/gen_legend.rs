//! Throwaway visual check for the labelled WMS legend (#371). Generates a few
//! legend PNGs to /tmp so they can be eyeballed for legibility.
//!
//! cargo run -p ds-render --example gen_legend

use ds_render::{render_legend, BuiltinColormap, ImageFormat, LutColorMap};

fn write_legend(
    name: &str,
    cmap: BuiltinColormap,
    min: f64,
    max: f64,
    title: Option<&str>,
    w: u32,
    h: u32,
) {
    let colormap = LutColorMap::from_builtin(cmap, min, max);
    let png = render_legend(&colormap, min, max, w, h, ImageFormat::Png, title).unwrap();
    let path = format!("/tmp/legend_{name}.png");
    std::fs::write(&path, &png).unwrap();
    println!("wrote {path} ({} bytes)", png.len());
}

fn main() {
    write_legend(
        "radar",
        BuiltinColormap::RadarDbz,
        -32.0,
        95.0,
        Some("DBZH (dBZ)"),
        180,
        300,
    );
    write_legend(
        "viridis",
        BuiltinColormap::Viridis,
        0.0,
        70.0,
        Some("reflectivity (dBZ)"),
        180,
        300,
    );
    write_legend(
        "temp",
        BuiltinColormap::Temperature,
        -30.0,
        40.0,
        Some("Temperature (degC)"),
        180,
        300,
    );
    write_legend("small", BuiltinColormap::Viridis, 0.0, 1.0, None, 40, 200);
    write_legend(
        "fraction",
        BuiltinColormap::Viridis,
        0.0,
        1.0,
        Some("cloud (%)"),
        160,
        260,
    );
}
