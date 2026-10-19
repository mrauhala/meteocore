//! The style a map request draws with, shared by WMS, Maps and Tiles so the
//! three cannot drift (#1133).
//!
//! The server registers one style map per style layer: the collection's own
//! under `"{collection}"`, and one per parameter under
//! `"{collection}/{parameter}"` for an engine that lists its parameters.
//! Only a parameter's map carries that parameter's colormap (from
//! `[[wms.parameters]]`, a bundle parameter entry, or a built-in parameter
//! default).

use std::collections::HashMap;
use std::fmt;

use ds_core::map_engine::RasterInfo;

use crate::StyleInfo;

/// A map request's style, as [`resolve_layer_style`] resolved it.
#[derive(Clone, Copy)]
pub struct LayerStyle<'a> {
    /// The style layer the style was found on, `"{collection}/{parameter}"`
    /// or `"{collection}"`. With the style's name it names the colormap, so
    /// the rendered and meta-tile caches key on it.
    pub layer: &'a str,
    /// The style.
    pub style: &'a StyleInfo,
    /// The parameter drawn: the one the request names, else the style's
    /// own, else the default parameter whose layer the style came from.
    /// `None` leaves it to the engine's default.
    pub parameter: Option<&'a str>,
}

impl fmt::Debug for LayerStyle<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LayerStyle")
            .field("layer", &self.layer)
            .field("style", &self.style.name)
            .field("palette", &self.style.palette.name)
            .field("parameter", &self.parameter)
            .finish()
    }
}

/// Why [`resolve_layer_style`] found no style.
pub enum LayerStyleError<'a> {
    /// The collection has no style layer.
    UnknownLayer,
    /// The layer has no style by that name; these are the styles it has.
    UnknownStyle(&'a HashMap<String, StyleInfo>),
}

impl fmt::Debug for LayerStyleError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownLayer => f.write_str("UnknownLayer"),
            Self::UnknownStyle(offered) => {
                let mut names: Vec<&str> = offered.keys().map(String::as_str).collect();
                names.sort_unstable();
                f.debug_tuple("UnknownStyle").field(&names).finish()
            }
        }
    }
}

/// Resolve the style `style` of a map request on `collection_id`.
///
/// - A request naming `parameter` (Maps/Tiles `parameter-name`, a WMS
///   `collection/parameter` layer) is styled from that parameter's layer,
///   else from the collection's, which a collection without parameter
///   layers relies on.
/// - A request naming no parameter draws the engine's default parameter,
///   `info.parameter`. The collection's layer decides which styles it has.
///   On a multi-parameter collection, one whose `info.parameters` lists
///   more than one, a style that draws that default (one that names no
///   parameter of its own) is taken from the default parameter's layer
///   when that layer has it. So the default parameter draws with its own
///   colormap, the one a request naming it gets, never the collection's: a
///   precipitation default on a temperature collection stays on the
///   precipitation palette.
/// - A single-parameter collection keeps the collection's style: its one
///   layer, as WMS advertises it, is the collection's, and the operator's
///   `[wms]` palette is the one it draws. With no palette configured, the
///   loader gives that layer its parameter's built-in default
///   (`StyleContext::base_layer_default`), since only the engine names
///   the parameter.
///
/// `info` is `None` when the collection has no map engine; the request is
/// then styled from the layer it names.
pub fn resolve_layer_style<'a>(
    styles: &'a HashMap<String, HashMap<String, StyleInfo>>,
    collection_id: &str,
    parameter: Option<&'a str>,
    info: Option<&'a RasterInfo>,
    style: &str,
) -> Result<LayerStyle<'a>, LayerStyleError<'a>> {
    let (layer, layer_styles) = parameter
        .and_then(|p| styles.get_key_value(format!("{collection_id}/{p}").as_str()))
        .or_else(|| styles.get_key_value(collection_id))
        .ok_or(LayerStyleError::UnknownLayer)?;
    let layer_style = layer_styles
        .get(style)
        .ok_or(LayerStyleError::UnknownStyle(layer_styles))?;

    let default_parameter = info
        .filter(|info| info.parameters.len() > 1)
        .map(|info| info.parameter.as_str())
        .filter(|p| !p.is_empty());
    if let (None, None, Some(default_parameter)) = (
        parameter,
        layer_style.parameter.as_deref(),
        default_parameter,
    ) {
        let default_layer = styles
            .get_key_value(format!("{collection_id}/{default_parameter}").as_str())
            .and_then(|(layer, layer_styles)| Some((layer, layer_styles.get(style)?)));
        if let Some((layer, style)) = default_layer {
            return Ok(LayerStyle {
                layer,
                style,
                parameter: Some(style.parameter.as_deref().unwrap_or(default_parameter)),
            });
        }
    }

    Ok(LayerStyle {
        layer,
        style: layer_style,
        parameter: parameter.or(layer_style.parameter.as_deref()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{StyleContext, StyleSpec};
    use ds_core::map_engine::ParameterInfo;

    fn style(name: &str, palette: &str, parameter: Option<&str>) -> StyleInfo {
        let resolved = StyleContext::with_builtins()
            .build_colormap(&StyleSpec {
                colormap: Some(palette),
                ..Default::default()
            })
            .unwrap();
        StyleInfo {
            name: name.to_string(),
            title: name.to_string(),
            colormap: resolved.colormap,
            palette: resolved.palette,
            min: resolved.min,
            max: resolved.max,
            parameter: parameter.map(str::to_string),
        }
    }

    /// The engine metadata of a collection whose default parameter is
    /// `default` among `parameters`.
    fn info(default: &str, parameters: &[&str]) -> RasterInfo {
        RasterInfo {
            native_crs: "CRS:84".to_string(),
            spatial_extent: None,
            times: Vec::new(),
            parameter: default.to_string(),
            unit: String::new(),
            parameters: parameters
                .iter()
                .map(|name| ParameterInfo {
                    name: name.to_string(),
                    title: name.to_string(),
                    unit: String::new(),
                })
                .collect(),
            vertical: None,
            grid_size: None,
            layer_subtitle: None,
            reference_times: Vec::new(),
        }
    }

    /// A GRIB-like collection: a temperature colormap at collection level,
    /// `tp` and `2t` with layers of their own. `pinned` is a named style
    /// for `2t` only, `shared` one every layer has, `base_only` one the
    /// parameter layers lack.
    fn styles() -> HashMap<String, HashMap<String, StyleInfo>> {
        HashMap::from([
            (
                "nwp".to_string(),
                HashMap::from([
                    ("default".into(), style("default", "temperature", None)),
                    ("shared".into(), style("shared", "grayscale", None)),
                    ("pinned".into(), style("pinned", "viridis", Some("2t"))),
                    ("base_only".into(), style("base_only", "viridis", None)),
                ]),
            ),
            (
                "nwp/tp".to_string(),
                HashMap::from([
                    (
                        "default".into(),
                        style("default", "precipitation", Some("tp")),
                    ),
                    ("shared".into(), style("shared", "grayscale", None)),
                ]),
            ),
            (
                "nwp/2t".to_string(),
                HashMap::from([
                    (
                        "default".into(),
                        style("default", "temperature", Some("2t")),
                    ),
                    ("shared".into(), style("shared", "grayscale", None)),
                    ("pinned".into(), style("pinned", "viridis", Some("2t"))),
                ]),
            ),
        ])
    }

    fn resolve<'a>(
        styles: &'a HashMap<String, HashMap<String, StyleInfo>>,
        parameter: Option<&'a str>,
        info: &'a RasterInfo,
        style: &str,
    ) -> LayerStyle<'a> {
        resolve_layer_style(styles, "nwp", parameter, Some(info), style)
            .unwrap_or_else(|error| panic!("style {style} resolves for {parameter:?}: {error:?}"))
    }

    /// The bug: a request naming no parameter drew the default `tp` with
    /// the collection's temperature palette. It now resolves exactly as a
    /// request naming `tp`: the same layer, style and parameter, so both
    /// share their cache keys.
    #[test]
    fn default_parameter_draws_with_its_own_style() {
        let styles = styles();
        let info = info("tp", &["tp", "2t"]);
        let unnamed = resolve(&styles, None, &info, "default");
        let named = resolve(&styles, Some("tp"), &info, "default");
        assert_eq!(unnamed.layer, "nwp/tp");
        assert_eq!(unnamed.style.palette.name, "precipitation");
        assert_eq!(unnamed.parameter, Some("tp"));
        assert_eq!(
            (
                unnamed.layer,
                unnamed.style.palette.name.as_str(),
                unnamed.parameter
            ),
            (
                named.layer,
                named.style.palette.name.as_str(),
                named.parameter
            )
        );
    }

    /// A shared named style draws the default parameter too, from its
    /// layer, and names it.
    #[test]
    fn named_style_on_the_default_layer_names_the_default_parameter() {
        let styles = styles();
        let info = info("tp", &["tp", "2t"]);
        let unnamed = resolve(&styles, None, &info, "shared");
        assert_eq!(unnamed.layer, "nwp/tp");
        assert_eq!(unnamed.parameter, Some("tp"));
    }

    /// A style that names its own parameter keeps the collection layer and
    /// draws that parameter, as before.
    #[test]
    fn style_naming_a_parameter_stays_on_the_collection_layer() {
        let styles = styles();
        let info = info("tp", &["tp", "2t"]);
        let pinned = resolve(&styles, None, &info, "pinned");
        assert_eq!(pinned.layer, "nwp");
        assert_eq!(pinned.parameter, Some("2t"));
    }

    /// A style the default parameter's layer lacks still resolves on the
    /// collection layer: the styles a request naming no parameter can
    /// choose are the collection's.
    #[test]
    fn style_missing_from_the_default_layer_stays_on_the_collection() {
        let styles = styles();
        let info = info("tp", &["tp", "2t"]);
        let base = resolve(&styles, None, &info, "base_only");
        assert_eq!(base.layer, "nwp");
        assert_eq!(base.parameter, None);
        assert!(matches!(
            resolve_layer_style(&styles, "nwp", Some("tp"), Some(&info), "base_only"),
            Err(LayerStyleError::UnknownStyle(_))
        ));
    }

    /// A default parameter without a layer of its own (one the parameter
    /// defaults do not style), or a collection without engine metadata,
    /// keeps the collection's style and leaves the parameter to the engine.
    #[test]
    fn default_parameter_without_a_layer_keeps_the_collection_style() {
        let styles = styles();
        for info in [info("msl", &["msl", "tp"]), info("", &["tp", "2t"])] {
            let resolved = resolve(&styles, None, &info, "default");
            assert_eq!(resolved.layer, "nwp", "{:?}", info.parameter);
            assert_eq!(resolved.style.palette.name, "temperature");
            assert_eq!(resolved.parameter, None);
        }
        let resolved = resolve_layer_style(&styles, "nwp", None, None, "default").unwrap();
        assert_eq!(resolved.layer, "nwp");
        assert_eq!(resolved.parameter, None);
    }

    /// A single-parameter collection's one layer is the collection's, and
    /// it draws with the collection's style even where the parameter's own
    /// layer matched a built-in parameter default. The style layers come
    /// from the server's resolution: a one-parameter `2t` collection in K
    /// whose `[wms]` palette is viridis gets a `c1/2t` layer on the
    /// built-in temperature default, which only a request naming `2t`
    /// draws.
    #[test]
    fn single_parameter_collection_keeps_the_collection_style() {
        let ctx = StyleContext::with_builtins();
        let collection: ds_core::config::CollectionConfig = toml::from_str(
            "id = \"c1\"\ntitle = \"t\"\ndescription = \"d\"\n[wms]\ncolormap = \"viridis\"\n",
        )
        .expect("test collection config parses");
        let mut info = info("2t", &["2t"]);
        // The temperature default is unit-gated.
        info.parameters[0].unit = "K".to_string();
        let mut styles = HashMap::from([(
            "c1".to_string(),
            ctx.collection_styles(&collection, None).unwrap(),
        )]);
        styles.extend(
            ctx.parameter_layer_styles(&collection, None, &info.parameters, &|_, cm| cm)
                .unwrap(),
        );
        assert_eq!(
            styles["c1/2t"]["default"].palette.name, "temperature",
            "the parameter layer matched the built-in default"
        );

        let resolved = resolve_layer_style(&styles, "c1", None, Some(&info), "default").unwrap();
        assert_eq!(resolved.layer, "c1");
        assert_eq!(resolved.style.palette.name, "viridis");
        assert_eq!(resolved.parameter, None);
    }

    /// A named parameter without a layer falls back to the collection and
    /// is still drawn, as before.
    #[test]
    fn named_parameter_without_a_layer_falls_back_to_the_collection() {
        let styles = styles();
        let info = info("tp", &["tp", "2t", "msl"]);
        let resolved = resolve(&styles, Some("msl"), &info, "default");
        assert_eq!(resolved.layer, "nwp");
        assert_eq!(resolved.parameter, Some("msl"));
    }

    #[test]
    fn unknown_collection_and_style_are_errors() {
        let styles = styles();
        let info = info("tp", &["tp", "2t"]);
        assert!(matches!(
            resolve_layer_style(&styles, "other", None, Some(&info), "default"),
            Err(LayerStyleError::UnknownLayer)
        ));
        match resolve_layer_style(&styles, "nwp", None, Some(&info), "nope") {
            Err(LayerStyleError::UnknownStyle(offered)) => {
                assert!(offered.contains_key("base_only"), "the collection's styles");
            }
            other => panic!("an unknown style is an error, got {other:?}"),
        }
    }
}
