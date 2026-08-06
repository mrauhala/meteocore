//! The single config→style resolution path.
//!
//! Everything that turns `[wms]` config (inline fields, `[[wms.styles]]`,
//! `[[wms.parameters]]`, style bundles) into render-ready [`StyleInfo`]
//! maps goes through [`StyleContext`], so the WMS/Maps/Tiles registries and
//! api-edr's `f=png` plots can never drift apart — they previously did:
//! the server and api-edr each carried their own copy of this logic.
//!
//! Engine-specific concerns stay out: the ODIM CELLS overlay wrap is
//! injected by the server through the `wrap` hook of
//! [`StyleContext::parameter_layer_styles`] (#410 will move it onto an
//! `OverlaySpec`).

use std::collections::HashMap;
use std::sync::Arc;

use ds_core::config::{CollectionConfig, StyleBundle, WmsParameterConfig};

use crate::colormap::{ColorMap, IntegerLutColorMap, LinearColorMap, LutColorMap};
use crate::palette::{Interpolation, Palette, PaletteRegistry};
use crate::{parse_hex_color, ColorStop, StyleInfo};

/// The style-affecting fields of one config block — `[wms]` inline fields,
/// one `[[wms.styles]]` entry, a bundle default/extra, or one
/// `[[wms.parameters]]` entry — reduced to a common shape.
#[derive(Clone, Copy, Default)]
pub struct StyleSpec<'a> {
    pub colormap: Option<&'a str>,
    pub color_stops: &'a [ds_core::config::ColorStop],
    pub min: Option<f64>,
    pub max: Option<f64>,
}

/// A resolved colormap: the render-ready (possibly integer-LUT-wrapped)
/// colormap, the palette it was built from (stops for legends), and the
/// effective value range.
#[derive(Clone)]
pub struct ResolvedColormap {
    pub colormap: Arc<dyn ColorMap>,
    pub palette: Arc<Palette>,
    pub min: f64,
    pub max: f64,
}

/// Resolver for config-declared styles against a palette registry.
///
/// Built once per config load (and per reload) by the server; api-edr
/// consumes the resolved [`StyleInfo`] maps, never raw config.
pub struct StyleContext {
    registry: PaletteRegistry,
}

impl StyleContext {
    pub fn new(registry: PaletteRegistry) -> Self {
        Self { registry }
    }

    /// A context with only the built-in palettes (no user `[[colormaps]]`).
    pub fn with_builtins() -> Self {
        Self::new(PaletteRegistry::with_builtins())
    }

    pub fn registry(&self) -> &PaletteRegistry {
        &self.registry
    }

    /// THE config→colormap path. Inline `color_stops` win; otherwise the
    /// named palette is resolved from the registry — an unknown name is an
    /// error (callers surface it at config load; a typo must not silently
    /// render viridis). No name at all falls back to viridis. `min`/`max`
    /// overrides default to the winning palette's first/last stop values.
    pub fn build_colormap(&self, spec: &StyleSpec) -> Result<ResolvedColormap, String> {
        // Inline custom stops take priority.
        if !spec.color_stops.is_empty() {
            let stops: Vec<ColorStop> = spec
                .color_stops
                .iter()
                .filter_map(|s| {
                    parse_hex_color(&s.color).ok().map(|color| ColorStop {
                        value: s.value,
                        color,
                    })
                })
                .collect();
            if !stops.is_empty() {
                // Palette::new sorts ascending — config stops may be
                // authored highest-first (the natural radar-dBZ legend
                // order), which would otherwise break the bracket lookup.
                let palette = Arc::new(Palette::new("custom", stops, Interpolation::Linear));
                let (min, max) = range_for(&palette, spec.min, spec.max);
                let colormap: Arc<dyn ColorMap> =
                    Arc::new(LinearColorMap::new(palette.stops.clone()));
                return Ok(ResolvedColormap {
                    colormap: maybe_wrap_integer_lut(colormap, min, max),
                    palette,
                    min,
                    max,
                });
            }
            // Every stop failed to hex-parse: fall through to the named /
            // default path (legacy behavior).
        }

        let name = spec.colormap.unwrap_or("viridis");
        let palette = self
            .registry
            .get(name)
            .ok_or_else(|| format!("unknown colormap '{name}'"))?;
        let (min, max) = range_for(&palette, spec.min, spec.max);
        let colormap: Arc<dyn ColorMap> = Arc::new(LutColorMap::from_palette(&palette, min, max));
        Ok(ResolvedColormap {
            colormap: maybe_wrap_integer_lut(colormap, min, max),
            palette,
            min,
            max,
        })
    }

    /// The collection-level default colormap: the bundle default when a
    /// bundle is bound, else the inline `[wms]` fields.
    pub fn collection_default(
        &self,
        collection: &CollectionConfig,
        bundle: Option<&StyleBundle>,
    ) -> Result<ResolvedColormap, String> {
        if let Some(bundle) = bundle {
            return self.build_colormap(&StyleSpec {
                colormap: bundle.default.colormap.as_deref(),
                color_stops: &bundle.default.color_stops,
                min: bundle.default.min,
                max: bundle.default.max,
            });
        }
        let wms = collection.wms.as_ref();
        self.build_colormap(&StyleSpec {
            colormap: wms.and_then(|w| w.colormap.as_deref()),
            color_stops: wms.map(|w| &w.color_stops[..]).unwrap_or(&[]),
            min: wms.and_then(|w| w.min),
            max: wms.and_then(|w| w.max),
        })
    }

    /// All styles for a collection's base layer: `default` plus named
    /// styles (bundle extras when a bundle is bound, else inline
    /// `[[wms.styles]]`).
    pub fn collection_styles(
        &self,
        collection: &CollectionConfig,
        bundle: Option<&StyleBundle>,
    ) -> Result<HashMap<String, StyleInfo>, String> {
        let mut styles = HashMap::new();

        let default = self.collection_default(collection, bundle)?;
        styles.insert(
            "default".to_string(),
            StyleInfo {
                name: "default".to_string(),
                title: "Default".to_string(),
                colormap: default.colormap,
                palette: default.palette,
                min: default.min,
                max: default.max,
                parameter: None,
            },
        );

        if let Some(bundle) = bundle {
            for extra in &bundle.extras {
                let r = self.build_colormap(&StyleSpec {
                    colormap: extra.colormap.as_deref(),
                    color_stops: &extra.color_stops,
                    min: extra.min,
                    max: extra.max,
                })?;
                styles.insert(
                    extra.name.clone(),
                    StyleInfo {
                        name: extra.name.clone(),
                        title: extra.title.clone().unwrap_or_else(|| extra.name.clone()),
                        colormap: r.colormap,
                        palette: r.palette,
                        min: r.min,
                        max: r.max,
                        parameter: extra.parameter.clone(),
                    },
                );
            }
        } else if let Some(wms_config) = &collection.wms {
            for style_config in &wms_config.styles {
                let r = self.build_colormap(&StyleSpec {
                    colormap: style_config.colormap.as_deref(),
                    color_stops: &style_config.color_stops,
                    min: style_config.min,
                    max: style_config.max,
                })?;
                styles.insert(
                    style_config.name.clone(),
                    StyleInfo {
                        name: style_config.name.clone(),
                        title: style_config
                            .title
                            .clone()
                            .unwrap_or_else(|| style_config.name.clone()),
                        colormap: r.colormap,
                        palette: r.palette,
                        min: r.min,
                        max: r.max,
                        parameter: style_config.parameter.clone(),
                    },
                );
            }
        }

        Ok(styles)
    }

    /// Per-parameter layer style maps (`"{collection}/{param}"` keys) for
    /// multi-parameter engines. Each parameter's default style uses its
    /// `[[wms.parameters]]` entry when configured, else the collection
    /// default; named styles are shared, except styles tagged with a
    /// specific `parameter`, which are scoped to that layer only.
    ///
    /// `wrap` lets the caller decorate a parameter's colormaps (the server
    /// wraps the ODIM CELLS overlay sentinel there — engine-specific logic
    /// that must not live in this crate; #410). It is called with the
    /// parameter short name and must return the (possibly wrapped) colormap.
    #[allow(clippy::type_complexity)]
    pub fn parameter_layer_styles(
        &self,
        collection: &CollectionConfig,
        bundle: Option<&StyleBundle>,
        param_names: &[(String, String)],
        wrap: &dyn Fn(&str, Arc<dyn ColorMap>) -> Arc<dyn ColorMap>,
    ) -> Result<HashMap<String, HashMap<String, StyleInfo>>, String> {
        let mut out = HashMap::new();
        let Some(wms_config) = &collection.wms else {
            return Ok(out);
        };

        let shared_named_styles = self.collection_styles(collection, bundle)?;

        // When a bundle is bound, inline per-parameter overrides are
        // rejected by validation (bundles v2 will merge them instead).
        let param_configs: HashMap<&str, &WmsParameterConfig> = wms_config
            .parameters
            .iter()
            .map(|p| (p.name.as_str(), p))
            .collect();

        let fallback = self.collection_default(collection, bundle)?;

        for (short_name, _title) in param_names {
            let layer_key = format!("{}/{}", collection.id, short_name);
            let mut layer_styles = HashMap::new();

            let r = if let Some(pc) = param_configs.get(short_name.as_str()) {
                self.build_colormap(&StyleSpec {
                    colormap: pc.colormap.as_deref(),
                    color_stops: &pc.color_stops,
                    min: pc.min,
                    max: pc.max,
                })?
            } else {
                fallback.clone()
            };

            layer_styles.insert(
                "default".to_string(),
                StyleInfo {
                    name: "default".to_string(),
                    title: "Default".to_string(),
                    colormap: wrap(short_name, r.colormap),
                    palette: r.palette,
                    min: r.min,
                    max: r.max,
                    parameter: Some(short_name.clone()),
                },
            );

            for (name, style) in &shared_named_styles {
                if name == "default" {
                    continue;
                }
                if let Some(p) = style.parameter.as_deref() {
                    if p != short_name {
                        continue;
                    }
                }
                layer_styles.insert(
                    name.clone(),
                    StyleInfo {
                        colormap: wrap(short_name, style.colormap.clone()),
                        ..style.clone()
                    },
                );
            }

            out.insert(layer_key, layer_styles);
        }

        Ok(out)
    }
}

/// `min`/`max` overrides fall back to the palette's first/last stop values
/// (and to 0..1 when the palette somehow has no stops), matching the
/// legacy resolution exactly.
fn range_for(
    palette: &Palette,
    min_override: Option<f64>,
    max_override: Option<f64>,
) -> (f64, f64) {
    let min = min_override.unwrap_or_else(|| palette.stops.first().map(|s| s.value).unwrap_or(0.0));
    let max = max_override.unwrap_or_else(|| palette.stops.last().map(|s| s.value).unwrap_or(1.0));
    (min, max)
}

/// Wrap `cmap` in [`IntegerLutColorMap`] when the value range fits a small
/// precomputed LUT (#207). Skipped for non-finite/inverted bounds, spans
/// below 16 integer steps (≥1-unit-per-stop is too coarse for sub-unit
/// gradients like viridis 0..1), or spans over the 65 536-entry cap.
fn maybe_wrap_integer_lut(cmap: Arc<dyn ColorMap>, min: f64, max: f64) -> Arc<dyn ColorMap> {
    if !min.is_finite() || !max.is_finite() {
        return cmap;
    }
    let lo = min.floor() as i64;
    let hi = max.ceil() as i64;
    match hi.checked_sub(lo) {
        Some(s) if (16..65_536).contains(&s) => {}
        _ => return cmap,
    }
    match IntegerLutColorMap::from_colormap(cmap.as_ref(), lo, hi) {
        Some(lut) => Arc::new(lut),
        // Unreachable given the (16..65_536) gate above (max span 65 535 →
        // range 65 536, which `from_colormap` accepts since its check is
        // `range > MAX_INTEGER_LUT_SIZE`). Kept as a safe fallback for the
        // day either bound is loosened.
        None => cmap,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::config::ColorStop as ConfigColorStop;

    fn spec_named(name: &str) -> StyleSpec<'_> {
        StyleSpec {
            colormap: Some(name),
            ..Default::default()
        }
    }

    #[test]
    fn named_builtin_resolves_with_stop_range() {
        let ctx = StyleContext::with_builtins();
        let r = ctx.build_colormap(&spec_named("radar_dbz")).unwrap();
        assert_eq!((r.min, r.max), (0.0, 70.0));
        assert_eq!(r.palette.name, "radar_dbz");
        // 0..70 spans ≥16 integer steps → integer-LUT wrapped, still
        // colours like the raw palette at integer values.
        assert_eq!(r.colormap.color(Some(35.0)), [255, 255, 0, 255]);
    }

    #[test]
    fn no_name_defaults_to_viridis_0_1() {
        let ctx = StyleContext::with_builtins();
        let r = ctx.build_colormap(&StyleSpec::default()).unwrap();
        assert_eq!(r.palette.name, "viridis");
        assert_eq!((r.min, r.max), (0.0, 1.0));
    }

    #[test]
    fn unknown_name_is_an_error() {
        let ctx = StyleContext::with_builtins();
        let Err(err) = ctx.build_colormap(&spec_named("virids")) else {
            panic!("typo'd colormap name must not resolve");
        };
        assert!(err.contains("virids"), "error names the typo: {err}");
    }

    #[test]
    fn inline_stops_win_over_name_and_sort_descending_input() {
        let ctx = StyleContext::with_builtins();
        let stops = vec![
            ConfigColorStop {
                value: 50.0,
                color: "#FF0000".into(),
            },
            ConfigColorStop {
                value: 0.0,
                color: "#000000".into(),
            },
        ];
        let r = ctx
            .build_colormap(&StyleSpec {
                colormap: Some("radar_dbz"),
                color_stops: &stops,
                min: None,
                max: None,
            })
            .unwrap();
        // Sorted ascending: range comes out 0..50, not 50..0.
        assert_eq!((r.min, r.max), (0.0, 50.0));
        assert_eq!(r.palette.stops[0].value, 0.0);
        assert_eq!(r.colormap.color(Some(50.0)), [255, 0, 0, 255]);
    }

    #[test]
    fn min_max_overrides_apply() {
        let ctx = StyleContext::with_builtins();
        let r = ctx
            .build_colormap(&StyleSpec {
                colormap: Some("viridis"),
                color_stops: &[],
                min: Some(200.0),
                max: Some(300.0),
            })
            .unwrap();
        assert_eq!((r.min, r.max), (200.0, 300.0));
    }

    // CollectionConfig is deserialized from TOML in production (per-file
    // collection configs use exactly this shape); build test instances the
    // same way rather than via a giant struct literal.
    fn coll(wms_toml: &str) -> CollectionConfig {
        let mut src = String::from("id = \"c1\"\ntitle = \"t\"\ndescription = \"d\"\n[wms]\n");
        src.push_str(wms_toml);
        toml::from_str(&src).expect("test collection config parses")
    }

    #[test]
    fn collection_styles_default_and_named() {
        let ctx = StyleContext::with_builtins();
        let c = coll(
            r#"
            colormap = "radar_dbz"
            [[wms.styles]]
            name = "gray"
            title = "Grayscale"
            colormap = "grayscale"
            min = 0.0
            max = 70.0
            "#,
        );
        let styles = ctx.collection_styles(&c, None).unwrap();
        assert_eq!(styles.len(), 2);
        assert_eq!(styles["default"].palette.name, "radar_dbz");
        assert_eq!(styles["gray"].title, "Grayscale");
        assert_eq!((styles["gray"].min, styles["gray"].max), (0.0, 70.0));
    }

    // --- maybe_wrap_integer_lut (#207) — moved here with the function from
    // the server's admin.rs when style resolution was consolidated. ---

    #[test]
    fn integer_lut_wraps_when_range_fits_and_is_wide_enough() {
        let src: Arc<dyn ColorMap> = Arc::new(LutColorMap::from_builtin(
            crate::BuiltinColormap::RadarDbz,
            -32.0,
            95.0,
        ));
        let wrapped = maybe_wrap_integer_lut(src.clone(), -32.0, 95.0);
        // It was replaced (no longer the same Arc).
        assert!(
            !Arc::ptr_eq(&src, &wrapped),
            "expected wrap on the radar_dbz -32..95 range"
        );
        // Colour at integer values matches the source — the LUT just precomputes.
        for v in [-32i64, -16, 0, 25, 50, 95] {
            assert_eq!(
                wrapped.color(Some(v as f64)),
                src.color(Some(v as f64)),
                "colour mismatch at v={v}"
            );
        }
        // Out-of-range saturates to the boundary entry (not transparent),
        // matching the float path's clamp — at integer endpoints we CAN
        // compare to src by construction.
        assert_eq!(wrapped.color(Some(-100.0)), src.color(Some(-32.0)));
        assert_eq!(wrapped.color(Some(200.0)), src.color(Some(95.0)));
        // (The non-integer rounding direction — round-nearest, not toward
        // zero — is pinned in the IntegerLutColorMap tests where the
        // colormap has distinct colours per integer. radar_dbz's low end is
        // transparent so it can't distinguish the directions here.)
    }

    #[test]
    fn integer_lut_skips_narrow_range() {
        // viridis 0..1 → only 2 integer entries → truncation would collapse the
        // gradient. Must NOT wrap.
        let src: Arc<dyn ColorMap> = Arc::new(LutColorMap::from_builtin(
            crate::BuiltinColormap::Viridis,
            0.0,
            1.0,
        ));
        let wrapped = maybe_wrap_integer_lut(src.clone(), 0.0, 1.0);
        assert!(
            Arc::ptr_eq(&src, &wrapped),
            "expected no wrap for a narrow (<16-entry) range"
        );
    }

    #[test]
    fn integer_lut_skips_huge_range() {
        // > 65 535 entries can't fit the integer LUT; fall back to the source.
        let src: Arc<dyn ColorMap> = Arc::new(LutColorMap::from_builtin(
            crate::BuiltinColormap::Viridis,
            -100_000.0,
            100_000.0,
        ));
        let wrapped = maybe_wrap_integer_lut(src.clone(), -100_000.0, 100_000.0);
        assert!(
            Arc::ptr_eq(&src, &wrapped),
            "expected no wrap for an over-cap range"
        );
    }

    #[test]
    fn integer_lut_skips_non_finite_bounds() {
        let src: Arc<dyn ColorMap> = Arc::new(LutColorMap::from_builtin(
            crate::BuiltinColormap::Viridis,
            0.0,
            1.0,
        ));
        assert!(Arc::ptr_eq(
            &src,
            &maybe_wrap_integer_lut(src.clone(), f64::NAN, 1.0)
        ));
        assert!(Arc::ptr_eq(
            &src,
            &maybe_wrap_integer_lut(src.clone(), 0.0, f64::INFINITY)
        ));
    }

    #[test]
    fn parameter_layer_styles_use_param_config_and_wrap_hook() {
        let ctx = StyleContext::with_builtins();
        let c = coll(
            r#"
            colormap = "radar_dbz"
            [[wms.parameters]]
            name = "VRADH"
            colormap = "radial_velocity"
            "#,
        );
        let params = vec![
            ("DBZH".to_string(), "Reflectivity".to_string()),
            ("VRADH".to_string(), "Radial velocity".to_string()),
        ];
        let wrapped_for: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let maps = ctx
            .parameter_layer_styles(&c, None, &params, &|short, cmap| {
                wrapped_for.borrow_mut().push(short.to_string());
                cmap
            })
            .unwrap();
        let wrapped_for = wrapped_for.into_inner();
        assert_eq!(maps.len(), 2);
        assert_eq!(maps["c1/DBZH"]["default"].palette.name, "radar_dbz");
        assert_eq!(maps["c1/VRADH"]["default"].palette.name, "radial_velocity");
        assert_eq!(
            maps["c1/VRADH"]["default"].parameter.as_deref(),
            Some("VRADH")
        );
        assert!(wrapped_for.contains(&"DBZH".to_string()));
        assert!(wrapped_for.contains(&"VRADH".to_string()));
    }
}
