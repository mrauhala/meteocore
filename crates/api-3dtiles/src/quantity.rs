//! How each quantity of a volume collection is drawn (#1137): its unit, the
//! colour ramp the point cloud and isosurface shells are painted with, and the
//! legend and suggested filter the collection document advertises.
//!
//! A quantity takes the default style its WMS/Maps/Tiles parameter layer
//! resolves to (`[[wms.parameters]]`, a style bundle, the built-in parameter
//! defaults, then the collection's `[wms]` fields), so a moment reads the same
//! in 2D and 3D. The server resolves those styles at load and hands them over
//! in [`crate::TilesState3d::quantity_styles`]; the legend is sampled here once
//! per load, never per request (#211).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock};

use ds_core::map_engine::ParameterInfo;
use ds_render::{BuiltinColormap, ColorMap, LutColorMap, StyleInfo};
use serde_json::{json, Value};

/// Reflectivity display range (dBZ), which the fallback ramp's legend covers:
/// 0–70 dBZ is the conventional radar display range and the whole coloured
/// part of the `radar_dbz` palette (it is transparent below 5 dBZ).
pub(crate) const REFLECTIVITY_RANGE: (f64, f64) = (0.0, 70.0);
const FALLBACK_LEGEND_MIN: f64 = REFLECTIVITY_RANGE.0;
const FALLBACK_LEGEND_MAX: f64 = REFLECTIVITY_RANGE.1;
/// Stops sampled per legend: enough for a smooth gradient bar.
const LEGEND_STOPS: usize = 24;
/// The unit that marks a reflectivity quantity.
const REFLECTIVITY_UNIT: &str = "dBZ";
/// Suggested point/voxel floor for reflectivity: the `radar_dbz` palette is
/// transparent at and below 5 dBZ, so weaker echo is clutter-grade noise the
/// ramp does not colour. Other quantities suggest no floor: a velocity or a
/// correlation coefficient has no "weak" end to drop.
const REFLECTIVITY_MIN_VALUE: f64 = 5.0;

/// The ramp for a quantity with no resolved style: the reflectivity ramp every
/// quantity used before per-quantity styles. The server passes it as
/// [`crate::TilesState3d::colormap`]; it colours only quantities a poll
/// brought in after the styles were resolved, until the next reload.
pub fn default_point_colormap() -> Arc<dyn ColorMap> {
    Arc::new(LutColorMap::from_builtin(
        BuiltinColormap::RadarDbz,
        -32.0,
        95.0,
    ))
}

/// Sample `colormap` into `(value, "#rrggbb")` stops across `[min, max]` for the
/// viewer legend. RGB only — `.pnts` colours are opaque, so alpha is dropped.
/// A non-finite or empty range yields no stops (the viewer then hides the bar).
fn legend_stops(colormap: &dyn ColorMap, min: f64, max: f64, n: usize) -> Vec<Value> {
    if !(min.is_finite() && max.is_finite() && max > min) {
        return Vec::new();
    }
    (0..n)
        .map(|i| {
            // Guard n == 1: `i/(n-1)` would be `0/0 = NaN`, which serde emits as
            // `null`.
            let v = min
                + if n <= 1 {
                    0.0
                } else {
                    (max - min) * (i as f64) / ((n - 1) as f64)
                };
            let c = colormap.color(Some(v));
            json!({
                // Rounded for display, with enough digits for a 0–1 range.
                "value": (v * 1000.0).round() / 1000.0,
                "color": format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]),
            })
        })
        .collect()
}

/// The fallback ramp's stops, sampled once.
static FALLBACK_STOPS: LazyLock<Vec<Value>> = LazyLock::new(|| {
    legend_stops(
        default_point_colormap().as_ref(),
        FALLBACK_LEGEND_MIN,
        FALLBACK_LEGEND_MAX,
        LEGEND_STOPS,
    )
});

/// How one quantity is drawn: built once per load from its resolved style.
pub struct QuantityStyle {
    unit: String,
    colormap: Arc<dyn ColorMap>,
    legend: Value,
    min_value: Option<f64>,
    fingerprint: u64,
}

impl QuantityStyle {
    /// From a parameter's resolved default style. `title` and `unit` are the
    /// parameter's (`ParameterInfo`), so the legend names the quantity.
    pub fn new(title: &str, unit: &str, style: &StyleInfo) -> Self {
        let legend = json!({
            "title": title,
            "unit": unit,
            "min": style.min,
            "max": style.max,
            "stops": legend_stops(style.colormap.as_ref(), style.min, style.max, LEGEND_STOPS),
        });
        Self {
            unit: unit.to_string(),
            colormap: style.colormap.clone(),
            legend,
            min_value: suggested_min(unit),
            fingerprint: style_fingerprint(style),
        }
    }

    /// For a quantity with no resolved style: the shared `colormap` over the
    /// reflectivity display range. `unit` is empty when it is unknown.
    fn fallback(colormap: &Arc<dyn ColorMap>, title: &str, unit: &str) -> Self {
        Self {
            unit: unit.to_string(),
            colormap: colormap.clone(),
            legend: json!({
                "title": title,
                "unit": unit,
                "min": FALLBACK_LEGEND_MIN,
                "max": FALLBACK_LEGEND_MAX,
                "stops": *FALLBACK_STOPS,
            }),
            min_value: suggested_min(unit),
            // The shared ramp is fixed for the process.
            fingerprint: 0,
        }
    }

    /// Physical unit of the quantity's values; empty when dimensionless or
    /// unknown.
    pub fn unit(&self) -> &str {
        &self.unit
    }

    /// The ramp `.pnts` points and isosurface shells are painted with.
    pub fn colormap(&self) -> &Arc<dyn ColorMap> {
        &self.colormap
    }

    /// `{title, unit, min, max, stops}`: the ramp sampled over its range, the
    /// gradient the viewer draws.
    pub fn legend(&self) -> &Value {
        &self.legend
    }

    /// The suggested floor for the point cloud and voxels, `None` when the
    /// quantity has no weak end to drop.
    pub fn min_value(&self) -> Option<f64> {
        self.min_value
    }

    /// Identifies the ramp, for the content cache key: a reload that restyles a
    /// quantity must not serve points painted with the old ramp.
    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }
}

fn suggested_min(unit: &str) -> Option<f64> {
    (unit == REFLECTIVITY_UNIT).then_some(REFLECTIVITY_MIN_VALUE)
}

/// A hash of everything that decides the colours a style paints: the palette
/// (name, interpolation, stops, nodata colour) and the range it spans.
fn style_fingerprint(style: &StyleInfo) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let palette = &style.palette;
    palette.name.hash(&mut h);
    palette.interpolation.as_str().hash(&mut h);
    palette.nodata_color.hash(&mut h);
    for stop in &palette.stops {
        stop.value.to_bits().hash(&mut h);
        stop.color.hash(&mut h);
    }
    style.min.to_bits().hash(&mut h);
    style.max.to_bits().hash(&mut h);
    // Never 0, the fallback ramp's fingerprint.
    h.finish().max(1)
}

/// One collection's quantity styles, keyed by quantity id.
pub type QuantityStyles = HashMap<String, Arc<QuantityStyle>>;

/// A collection's quantity styles from its resolved parameter-layer styles
/// (keyed `"{collection}/{parameter}"`, each with a `default` style, as the
/// WMS/Maps/Tiles registries hold them). A parameter with no resolved layer is
/// left out and draws with the fallback ramp.
pub fn quantity_styles(
    collection: &str,
    parameters: &[ParameterInfo],
    layer_styles: &HashMap<String, HashMap<String, StyleInfo>>,
) -> QuantityStyles {
    parameters
        .iter()
        .filter_map(|p| {
            let style = layer_styles
                .get(&format!("{collection}/{}", p.name))?
                .get("default")?;
            Some((
                p.name.clone(),
                Arc::new(QuantityStyle::new(&p.title, &p.unit, style)),
            ))
        })
        .collect()
}

/// The style a quantity draws with: its resolved one, else the fallback ramp.
/// Only the default quantity's unit is known without a resolved style
/// (`VolumeInfo::default_unit`).
pub(crate) fn resolve(
    state: &crate::TilesState3d,
    collection: &str,
    info: &ds_core::volume::VolumeInfo,
    quantity: &str,
    label: &str,
) -> Arc<QuantityStyle> {
    if let Some(style) = state
        .quantity_styles
        .get(collection)
        .and_then(|styles| styles.get(quantity))
    {
        return style.clone();
    }
    let unit = if quantity == info.default_quantity {
        info.default_unit.as_str()
    } else {
        ""
    };
    Arc::new(QuantityStyle::fallback(&state.colormap, label, unit))
}

/// The ramp a content request paints `quantity` with, and its fingerprint for
/// the cache key — the lookup without building a fallback legend.
pub(crate) fn ramp(
    state: &crate::TilesState3d,
    collection: &str,
    quantity: &str,
) -> (Arc<dyn ColorMap>, u64) {
    match state
        .quantity_styles
        .get(collection)
        .and_then(|styles| styles.get(quantity))
    {
        Some(style) => (style.colormap.clone(), style.fingerprint),
        None => (state.colormap.clone(), 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_render::{Interpolation, Palette};

    fn style(palette: Palette, min: f64, max: f64) -> StyleInfo {
        let colormap: Arc<dyn ColorMap> = Arc::new(LutColorMap::from_palette(&palette, min, max));
        StyleInfo {
            name: "default".into(),
            title: "Default".into(),
            colormap,
            palette: Arc::new(palette),
            min,
            max,
            parameter: None,
        }
    }

    fn velocity() -> StyleInfo {
        style(
            Palette::new(
                "custom",
                vec![
                    ds_render::ColorStop {
                        value: -48.0,
                        color: [26, 26, 255, 255],
                    },
                    ds_render::ColorStop {
                        value: 0.0,
                        color: [245, 245, 245, 255],
                    },
                    ds_render::ColorStop {
                        value: 48.0,
                        color: [204, 0, 0, 255],
                    },
                ],
                Interpolation::Linear,
            ),
            -48.0,
            48.0,
        )
    }

    /// A velocity legend spans the style's own range in m/s, its ends carry
    /// the palette's end colours, and velocity suggests no floor.
    #[test]
    fn legend_spans_the_style_range_in_the_quantity_unit() {
        let q = QuantityStyle::new("VRADH — Radial velocity (horizontal)", "m/s", &velocity());
        let lg = q.legend();
        assert_eq!(lg["unit"], "m/s");
        assert_eq!(lg["title"], "VRADH — Radial velocity (horizontal)");
        assert_eq!(lg["min"], -48.0);
        assert_eq!(lg["max"], 48.0);
        let stops = lg["stops"].as_array().unwrap();
        assert_eq!(stops.len(), LEGEND_STOPS);
        assert_eq!(stops[0]["value"], -48.0);
        assert_eq!(stops[0]["color"], "#1a1aff");
        assert_eq!(stops[LEGEND_STOPS - 1]["value"], 48.0);
        assert_eq!(stops[LEGEND_STOPS - 1]["color"], "#cc0000");
        assert_eq!(q.min_value(), None);
        assert_eq!(q.unit(), "m/s");
    }

    /// Reflectivity suggests the 5 dBZ floor the viewer has always defaulted
    /// to; a 0–1 quantity keeps its fractional stop values.
    #[test]
    fn reflectivity_suggests_a_floor_and_fractions_keep_their_values() {
        let dbz = style(
            ds_render::palette::builtin_palette("radar_dbz")
                .unwrap()
                .clone(),
            0.0,
            70.0,
        );
        assert_eq!(
            QuantityStyle::new("DBZH", "dBZ", &dbz).min_value(),
            Some(5.0)
        );
        let rhohv = style(
            ds_render::palette::builtin_palette("viridis")
                .unwrap()
                .stretched(0.0, 1.0),
            0.0,
            1.0,
        );
        let q = QuantityStyle::new("RHOHV", "", &rhohv);
        assert_eq!(q.min_value(), None);
        let values: Vec<f64> = q.legend()["stops"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["value"].as_f64().unwrap())
            .collect();
        assert_eq!(values[0], 0.0);
        assert_eq!(values[1], 0.043); // 1/23, not rounded to 0
        assert_eq!(*values.last().unwrap(), 1.0);
    }

    /// Restyling a quantity changes its fingerprint (the cache key); the same
    /// style always gives the same one, and never the fallback's 0.
    #[test]
    fn fingerprint_follows_the_ramp() {
        let a = style_fingerprint(&velocity());
        assert_eq!(a, style_fingerprint(&velocity()));
        assert_ne!(a, 0);
        let mut wider = velocity();
        wider.max = 60.0;
        assert_ne!(a, style_fingerprint(&wider));
    }

    /// Parameters with a resolved layer style get it; one without is left to
    /// the fallback.
    #[test]
    fn styles_come_from_the_parameter_layers() {
        let mut layers = HashMap::new();
        layers.insert(
            "site/VRADH".to_string(),
            HashMap::from([("default".to_string(), velocity())]),
        );
        let params = [
            ParameterInfo {
                name: "VRADH".into(),
                title: "Radial velocity".into(),
                unit: "m/s".into(),
            },
            ParameterInfo {
                name: "ZDR".into(),
                title: "Differential reflectivity".into(),
                unit: "dB".into(),
            },
        ];
        let styles = quantity_styles("site", &params, &layers);
        assert_eq!(styles.len(), 1);
        assert_eq!(styles["VRADH"].unit(), "m/s");
    }
}
