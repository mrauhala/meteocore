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
    defaults: crate::defaults::ParameterDefaults,
}

impl StyleContext {
    pub fn new(registry: PaletteRegistry) -> Self {
        Self {
            registry,
            defaults: crate::defaults::ParameterDefaults::default(),
        }
    }

    /// A context with only the built-in palettes (no user `[[colormaps]]`).
    pub fn with_builtins() -> Self {
        Self::new(PaletteRegistry::with_builtins())
    }

    /// Attach config `[[parameter_defaults]]` override rules (checked
    /// before the embedded table).
    pub fn with_defaults(mut self, defaults: crate::defaults::ParameterDefaults) -> Self {
        self.defaults = defaults;
        self
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

    /// The collection-level default colormap: the inline `[wms]` fields
    /// merged slot-wise over the bundle default (bundles v2) — inline wins
    /// each slot it defines, so a collection can bind a shared bundle and
    /// override only e.g. `min`/`max`.
    pub fn collection_default(
        &self,
        collection: &CollectionConfig,
        bundle: Option<&StyleBundle>,
    ) -> Result<ResolvedColormap, String> {
        self.build_colormap(&self.collection_default_spec(collection, bundle))
    }

    /// The merged (inline-over-bundle) spec behind [`collection_default`].
    fn collection_default_spec<'a>(
        &self,
        collection: &'a CollectionConfig,
        bundle: Option<&'a StyleBundle>,
    ) -> StyleSpec<'a> {
        let wms = collection.wms.as_ref();
        let inline = StyleSpec {
            colormap: wms.and_then(|w| w.colormap.as_deref()),
            color_stops: wms.map(|w| &w.color_stops[..]).unwrap_or(&[]),
            min: wms.and_then(|w| w.min),
            max: wms.and_then(|w| w.max),
        };
        let Some(bundle) = bundle else { return inline };
        merge_specs(
            inline,
            StyleSpec {
                colormap: bundle.default.colormap.as_deref(),
                color_stops: &bundle.default.color_stops,
                min: bundle.default.min,
                max: bundle.default.max,
            },
        )
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

        // Named styles: union of bundle extras and inline [[wms.styles]]
        // (bundles v2) — inline wins on a name clash because it is inserted
        // second into the same map.
        if let Some(bundle) = bundle {
            for extra in &bundle.extras {
                // Name defaults to the colormap reference (validated to
                // exist); title falls back to the palette's own title, so a
                // pure palette reference needs only `colormap = "..."`.
                let Some(name) = extra.effective_name().map(str::to_string) else {
                    continue; // rejected by validation; defensive
                };
                let r = self.build_colormap(&StyleSpec {
                    colormap: extra.colormap.as_deref(),
                    color_stops: &extra.color_stops,
                    min: extra.min,
                    max: extra.max,
                })?;
                let title = extra
                    .title
                    .clone()
                    .or_else(|| r.palette.title.clone())
                    .unwrap_or_else(|| name.clone());
                styles.insert(
                    name.clone(),
                    StyleInfo {
                        name,
                        title,
                        colormap: r.colormap,
                        palette: r.palette,
                        min: r.min,
                        max: r.max,
                        parameter: extra.parameter.clone(),
                    },
                );
            }
        }
        if let Some(wms_config) = &collection.wms {
            for style_config in &wms_config.styles {
                let Some(name) = style_config.effective_name().map(str::to_string) else {
                    continue; // no name and no colormap to derive one from
                };
                let r = self.build_colormap(&StyleSpec {
                    colormap: style_config.colormap.as_deref(),
                    color_stops: &style_config.color_stops,
                    min: style_config.min,
                    max: style_config.max,
                })?;
                let title = style_config
                    .title
                    .clone()
                    .or_else(|| r.palette.title.clone())
                    .unwrap_or_else(|| name.clone());
                styles.insert(
                    name.clone(),
                    StyleInfo {
                        name,
                        title,
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
        unit: Option<&str>,
        wrap: &dyn Fn(&str, Arc<dyn ColorMap>) -> Arc<dyn ColorMap>,
    ) -> Result<HashMap<String, HashMap<String, StyleInfo>>, String> {
        let mut out = HashMap::new();

        let shared_named_styles = self.collection_styles(collection, bundle)?;

        // Per-parameter chain (bundles v2 + parameter defaults #320), each
        // slot resolved independently, first level that defines it wins:
        //   1. inline [[wms.parameters]] entry
        //   2. bundle [[style_bundles.parameters]] entry
        //   3. built-in / [[parameter_defaults]] match (multi-param
        //      collections; the match REPLACES levels 4-5 as one atomic
        //      base so a default palette can't pick up an unrelated
        //      collection-level min/max)
        //   4. inline [wms] collection fields
        //   5. bundle default
        // A collection can opt out of level 3 with
        // `[wms] parameter_defaults = false`.
        let param_configs: HashMap<&str, &WmsParameterConfig> = collection
            .wms
            .as_ref()
            .map(|w| w.parameters.iter().map(|p| (p.name.as_str(), p)).collect())
            .unwrap_or_default();
        let bundle_params: HashMap<&str, &WmsParameterConfig> = bundle
            .map(|b| b.parameters.iter().map(|p| (p.name.as_str(), p)).collect())
            .unwrap_or_default();
        let defaults_enabled = collection
            .wms
            .as_ref()
            .and_then(|w| w.parameter_defaults)
            .unwrap_or(true);

        let collection_spec = self.collection_default_spec(collection, bundle);
        let fallback = self.build_colormap(&collection_spec)?;

        fn param_spec(pc: &WmsParameterConfig) -> StyleSpec<'_> {
            StyleSpec {
                colormap: pc.colormap.as_deref(),
                color_stops: &pc.color_stops,
                min: pc.min,
                max: pc.max,
            }
        }

        for (short_name, title) in param_names {
            let layer_key = format!("{}/{}", collection.id, short_name);
            let mut layer_styles = HashMap::new();

            let inline_pc = param_configs.get(short_name.as_str());
            let bundle_pc = bundle_params.get(short_name.as_str());
            let matched = if defaults_enabled {
                self.defaults.match_default(short_name, title, unit)
            } else {
                None
            };
            let r = if inline_pc.is_some() || bundle_pc.is_some() || matched.is_some() {
                let base = match &matched {
                    Some(d) => StyleSpec {
                        colormap: Some(&d.palette),
                        color_stops: &[],
                        min: d.range.map(|r| r.0),
                        max: d.range.map(|r| r.1),
                    },
                    None => collection_spec,
                };
                let mut spec = base;
                if let Some(pc) = bundle_pc {
                    spec = merge_specs(param_spec(pc), spec);
                }
                if let Some(pc) = inline_pc {
                    spec = merge_specs(param_spec(pc), spec);
                }
                self.build_colormap(&spec)?
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

/// Slot-wise merge (bundles v2): `primary` wins every slot it defines. The
/// palette source — the (colormap name, color_stops) pair — is ONE slot: if
/// primary defines either, primary's whole pair is taken (mixing primary
/// stops with a fallback name would be incoherent); `min` and `max` merge
/// independently, so an override of just the range inherits the palette.
fn merge_specs<'a>(primary: StyleSpec<'a>, fallback: StyleSpec<'a>) -> StyleSpec<'a> {
    let primary_has_palette = primary.colormap.is_some() || !primary.color_stops.is_empty();
    let (colormap, color_stops) = if primary_has_palette {
        (primary.colormap, primary.color_stops)
    } else {
        (fallback.colormap, fallback.color_stops)
    };
    StyleSpec {
        colormap,
        color_stops,
        min: primary.min.or(fallback.min),
        max: primary.max.or(fallback.max),
    }
}

/// `min`/`max` overrides fall back to the palette's stop values — `max`
/// from the last stop, `min` from [`Palette::domain_min_stop`] (the first
/// stop, except a `.pal` display-threshold guard sitting one ULP below
/// the real threshold) — and to 0..1 when the palette somehow has no
/// stops.
fn range_for(
    palette: &Palette,
    min_override: Option<f64>,
    max_override: Option<f64>,
) -> (f64, f64) {
    // The derived minimum skips a .pal display-threshold guard stop (one
    // ULP below the real threshold) — see Palette::domain_min_stop.
    let min =
        min_override.unwrap_or_else(|| palette.domain_min_stop().map(|s| s.value).unwrap_or(0.0));
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

    fn bundle(toml_src: &str) -> StyleBundle {
        toml::from_str(toml_src).expect("test bundle parses")
    }

    /// Bundles v2: a bundle carries per-parameter defaults; collections
    /// inherit them with zero inline config (the nexus radar-volume case
    /// that previously required ~525 copy-pasted lines per file).
    #[test]
    fn bundle_parameters_shared_across_collections() {
        let ctx = StyleContext::with_builtins();
        let b = bundle(
            r#"
            id = "radar_volume"
            [default]
            colormap = "radar_dbz"
            [[parameters]]
            name = "VRADH"
            colormap = "radial_velocity"
            min = -48.0
            max = 48.0
            [[extras]]
            name = "gray"
            colormap = "grayscale"
            min = 0.0
            max = 70.0
            "#,
        );
        let params = vec![
            ("DBZH".to_string(), "Z".to_string()),
            ("VRADH".to_string(), "V".to_string()),
        ];
        for id in ["c1", "c2"] {
            let mut c = coll("style_bundle = \"radar_volume\"\n");
            c.id = id.to_string();
            let maps = ctx
                .parameter_layer_styles(&c, Some(&b), &params, None, &|_, cm| cm)
                .unwrap();
            let dbzh = &maps[&format!("{id}/DBZH")]["default"];
            assert_eq!(dbzh.palette.name, "radar_dbz");
            let vradh = &maps[&format!("{id}/VRADH")]["default"];
            assert_eq!(vradh.palette.name, "radial_velocity");
            assert_eq!((vradh.min, vradh.max), (-48.0, 48.0));
            // Bundle extras present on parameter layers too.
            assert!(maps[&format!("{id}/VRADH")].contains_key("gray"));
        }
    }

    /// Slot-wise merge: inline defines only min/max → palette inherited
    /// from the bundle; an inline parameter entry overrides the bundle's
    /// parameter entry per slot.
    #[test]
    fn slot_wise_merge_inline_over_bundle() {
        let ctx = StyleContext::with_builtins();
        let b = bundle(
            r#"
            id = "vol"
            [default]
            colormap = "radar_dbz"
            [[parameters]]
            name = "VRADH"
            colormap = "radial_velocity"
            min = -48.0
            max = 48.0
            "#,
        );
        // Collection overrides ONLY the range of the default style…
        let c = coll(
            r#"
            style_bundle = "vol"
            min = -10.0
            max = 80.0
            [[wms.parameters]]
            name = "VRADH"
            min = -24.0
            max = 24.0
            "#,
        );
        let default = ctx.collection_default(&c, Some(&b)).unwrap();
        // …palette slot inherited from the bundle, range from inline.
        assert_eq!(default.palette.name, "radar_dbz");
        assert_eq!((default.min, default.max), (-10.0, 80.0));

        let params = vec![("VRADH".to_string(), "V".to_string())];
        let maps = ctx
            .parameter_layer_styles(&c, Some(&b), &params, None, &|_, cm| cm)
            .unwrap();
        let vradh = &maps["c1/VRADH"]["default"];
        // Inline param defines only min/max → narrows the range, inherits
        // the bundle parameter's palette.
        assert_eq!(vradh.palette.name, "radial_velocity");
        assert_eq!((vradh.min, vradh.max), (-24.0, 24.0));
    }

    /// A pure palette reference needs only `colormap = "..."`: the style
    /// name defaults to the colormap name, the title to the palette's title.
    #[test]
    fn one_line_extra_defaults_name_and_title_from_palette() {
        let ctx = StyleContext::with_builtins();
        let b = bundle(
            r#"
            id = "b"
            [default]
            colormap = "radar_bookbinder"
            [[extras]]
            colormap = "radar_dbz"
            [[extras]]
            title = "House gray"
            colormap = "grayscale"
            "#,
        );
        let c = coll("style_bundle = \"b\"\n");
        let styles = ctx.collection_styles(&c, Some(&b)).unwrap();
        let dbz = &styles["radar_dbz"];
        assert_eq!(dbz.name, "radar_dbz");
        assert_eq!(dbz.title, "Radar reflectivity (dBZ)"); // palette title
                                                           // Explicit title still wins over the palette's.
        assert_eq!(styles["grayscale"].title, "House gray");

        // Inline [[wms.styles]] one-liner behaves identically.
        let c = coll("colormap = \"viridis\"\n[[wms.styles]]\ncolormap = \"temperature\"\n");
        let styles = ctx.collection_styles(&c, None).unwrap();
        assert_eq!(styles["temperature"].title, "Temperature (°C)");
    }

    /// Named styles are the union of bundle extras and inline styles;
    /// inline wins a name clash.
    #[test]
    fn named_styles_union_inline_wins() {
        let ctx = StyleContext::with_builtins();
        let b = bundle(
            r#"
            id = "vol"
            [default]
            colormap = "radar_dbz"
            [[extras]]
            name = "gray"
            colormap = "grayscale"
            min = 0.0
            max = 1.0
            [[extras]]
            name = "fmi"
            colormap = "radar_fmi"
            "#,
        );
        let c = coll(
            r#"
            style_bundle = "vol"
            [[wms.styles]]
            name = "gray"
            title = "Wide gray"
            colormap = "grayscale"
            min = -32.0
            max = 95.0
            [[wms.styles]]
            name = "local_extra"
            colormap = "viridis"
            "#,
        );
        let styles = ctx.collection_styles(&c, Some(&b)).unwrap();
        assert_eq!(styles.len(), 4); // default + gray + fmi + local_extra
        assert_eq!((styles["gray"].min, styles["gray"].max), (-32.0, 95.0));
        assert_eq!(styles["gray"].title, "Wide gray");
        assert!(styles.contains_key("fmi"));
        assert!(styles.contains_key("local_extra"));
    }

    /// #320: parameters of a multi-parameter collection match built-in
    /// defaults BEFORE the collection-level colormap (the meps-surface fix).
    #[test]
    fn parameter_defaults_beat_collection_colormap_on_multi_param() {
        let ctx = StyleContext::with_builtins();
        let c = coll("colormap = \"temperature\"\n");
        let params = vec![
            ("t2m".to_string(), "2 m temperature".to_string()),
            ("msl".to_string(), "Mean sea-level pressure".to_string()),
            ("zzz".to_string(), "Mystery".to_string()),
        ];
        let maps = ctx
            .parameter_layer_styles(&c, None, &params, Some("hPa"), &|_, cm| cm)
            .unwrap();
        // msl matches the pressure default despite the collection colormap.
        let msl = &maps["c1/msl"]["default"];
        assert_eq!(msl.palette.name, "pressure");
        assert_eq!((msl.min, msl.max), (950.0, 1050.0));
        // Unmatched parameter falls back to the collection colormap.
        assert_eq!(maps["c1/zzz"]["default"].palette.name, "temperature");
        // t2m: the collection-level unit hint is hPa; the temperature rule
        // is unit-gated (never guess K vs C) → no default, collection wins.
        assert_eq!(maps["c1/t2m"]["default"].palette.name, "temperature");
    }

    #[test]
    fn parameter_defaults_opt_out_and_inline_override() {
        let ctx = StyleContext::with_builtins();
        let params = vec![("msl".to_string(), "MSLP".to_string())];

        // Opt-out: collection colormap paints everything, as before.
        let c = coll("colormap = \"temperature\"\nparameter_defaults = false\n");
        let maps = ctx
            .parameter_layer_styles(&c, None, &params, Some("hPa"), &|_, cm| cm)
            .unwrap();
        assert_eq!(maps["c1/msl"]["default"].palette.name, "temperature");

        // Inline [[wms.parameters]] beats the default.
        let c = coll(
            "colormap = \"temperature\"\n[[wms.parameters]]\nname = \"msl\"\ncolormap = \"viridis\"\nmin = 980.0\nmax = 1040.0\n",
        );
        let maps = ctx
            .parameter_layer_styles(&c, None, &params, Some("hPa"), &|_, cm| cm)
            .unwrap();
        assert_eq!(maps["c1/msl"]["default"].palette.name, "viridis");
        assert_eq!(
            (maps["c1/msl"]["default"].min, maps["c1/msl"]["default"].max),
            (980.0, 1040.0)
        );

        // Inline param with only a range narrows the DEFAULT palette.
        let c = coll("[[wms.parameters]]\nname = \"msl\"\nmin = 990.0\nmax = 1030.0\n");
        let maps = ctx
            .parameter_layer_styles(&c, None, &params, Some("hPa"), &|_, cm| cm)
            .unwrap();
        let msl = &maps["c1/msl"]["default"];
        assert_eq!(msl.palette.name, "pressure");
        assert_eq!((msl.min, msl.max), (990.0, 1030.0));
    }

    /// Zero-config (#320): a multi-parameter collection with NO [wms] block
    /// still gets sensibly-styled parameter layers.
    #[test]
    fn no_wms_block_multi_param_gets_default_layers() {
        let ctx = StyleContext::with_builtins();
        let c: CollectionConfig =
            toml::from_str("id = \"c1\"\ntitle = \"t\"\ndescription = \"d\"\n").unwrap();
        let params = vec![("DBZH".to_string(), "Reflectivity".to_string())];
        let maps = ctx
            .parameter_layer_styles(&c, None, &params, Some("dBZ"), &|_, cm| cm)
            .unwrap();
        assert_eq!(maps["c1/DBZH"]["default"].palette.name, "radar_dbz");
        assert_eq!(
            (
                maps["c1/DBZH"]["default"].min,
                maps["c1/DBZH"]["default"].max
            ),
            (0.0, 70.0)
        );
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
            .parameter_layer_styles(&c, None, &params, None, &|short, cmap| {
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
