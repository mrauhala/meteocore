use std::collections::HashMap;
use std::sync::Arc;

use ds_core::config::{CollectionConfig, LicenseConfig};
use ds_core::map_engine::{MapEngine, RasterInfo};
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

use crate::params;
use ds_render::StyleInfo;

/// Generate WMS 1.3.0 GetCapabilities XML.
///
/// Uses quick-xml Writer to ensure all text content is properly escaped.
/// Never builds XML via format!() or string concatenation (XML injection risk).
pub fn get_capabilities_xml(
    engines: &HashMap<String, Arc<dyn MapEngine>>,
    collections: &HashMap<String, CollectionConfig>,
    styles: &HashMap<String, HashMap<String, StyleInfo>>,
    base_url: &str,
) -> Vec<u8> {
    let mut writer = Writer::new(Vec::new());

    // XML declaration
    let _ = writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)));

    // Root element
    let mut root = BytesStart::new("WMS_Capabilities");
    root.push_attribute(("version", "1.3.0"));
    root.push_attribute(("xmlns", "http://www.opengis.net/wms"));
    root.push_attribute(("xmlns:xlink", "http://www.w3.org/1999/xlink"));
    let _ = writer.write_event(Event::Start(root));

    // Service section
    write_service(&mut writer);

    // Capability section
    let _ = writer.write_event(Event::Start(BytesStart::new("Capability")));
    write_request(&mut writer, base_url);

    // Root Layer (container for all layers)
    let _ = writer.write_event(Event::Start(BytesStart::new("Layer")));
    write_text_element(&mut writer, "Title", "MeteoCore - WMS");

    // Supported CRS
    for crs in params::supported_crs_list() {
        write_text_element(&mut writer, "CRS", crs);
    }

    // Individual layers (one per collection)
    for (id, config) in collections {
        if let Some(engine) = engines.get(id) {
            let layer_styles = styles.get(id.as_str());
            let info = engine.raster_info();

            if info.parameters.len() > 1 {
                // Multi-parameter engine: parent layer (not requestable) with
                // nested child layers per parameter
                write_parent_layer(&mut writer, id, config, &info, layer_styles, base_url);
            } else {
                // Single-parameter engine: one requestable layer
                write_layer(&mut writer, id, config, &info, layer_styles, base_url, None);
            }
        }
    }

    let _ = writer.write_event(Event::End(BytesEnd::new("Layer"))); // root Layer
    let _ = writer.write_event(Event::End(BytesEnd::new("Capability")));
    let _ = writer.write_event(Event::End(BytesEnd::new("WMS_Capabilities")));

    writer.into_inner()
}

fn write_service(writer: &mut Writer<Vec<u8>>) {
    let _ = writer.write_event(Event::Start(BytesStart::new("Service")));
    write_text_element(writer, "Name", "WMS");
    write_text_element(writer, "Title", "MeteoCore - WMS");
    write_text_element(writer, "Abstract", "Metocean Data Server — OGC WMS 1.3.0");
    let _ = writer.write_event(Event::End(BytesEnd::new("Service")));
}

fn write_request(writer: &mut Writer<Vec<u8>>, base_url: &str) {
    let _ = writer.write_event(Event::Start(BytesStart::new("Request")));

    // GetCapabilities
    let _ = writer.write_event(Event::Start(BytesStart::new("GetCapabilities")));
    write_format(writer, "text/xml");
    write_dcp_type(writer, base_url);
    let _ = writer.write_event(Event::End(BytesEnd::new("GetCapabilities")));

    // GetMap and GetLegendGraphic accept the same image formats — iterate
    // SUPPORTED_FORMATS so the advertised list can't drift from what the
    // handlers actually serve.
    for op in ["GetMap", "GetLegendGraphic"] {
        let _ = writer.write_event(Event::Start(BytesStart::new(op)));
        for format in params::SUPPORTED_FORMATS {
            write_format(writer, format);
        }
        write_dcp_type(writer, base_url);
        let _ = writer.write_event(Event::End(BytesEnd::new(op)));
    }

    let _ = writer.write_event(Event::End(BytesEnd::new("Request")));
}

fn write_format(writer: &mut Writer<Vec<u8>>, format: &str) {
    write_text_element(writer, "Format", format);
}

fn write_dcp_type(writer: &mut Writer<Vec<u8>>, base_url: &str) {
    let _ = writer.write_event(Event::Start(BytesStart::new("DCPType")));
    let _ = writer.write_event(Event::Start(BytesStart::new("HTTP")));
    let _ = writer.write_event(Event::Start(BytesStart::new("Get")));

    let wms_url = format!("{base_url}/wms?");
    let mut or = BytesStart::new("OnlineResource");
    or.push_attribute(("xlink:type", "simple"));
    or.push_attribute(("xlink:href", wms_url.as_str()));
    let _ = writer.write_event(Event::Empty(or));

    let _ = writer.write_event(Event::End(BytesEnd::new("Get")));
    let _ = writer.write_event(Event::End(BytesEnd::new("HTTP")));
    let _ = writer.write_event(Event::End(BytesEnd::new("DCPType")));
}

/// Write a parent layer for a multi-parameter collection.
/// The parent is not directly requestable — child layers per parameter are.
fn write_parent_layer(
    writer: &mut Writer<Vec<u8>>,
    id: &str,
    config: &CollectionConfig,
    info: &RasterInfo,
    layer_styles: Option<&HashMap<String, StyleInfo>>,
    base_url: &str,
) {
    let _ = writer.write_event(Event::Start(BytesStart::new("Layer")));

    // Parent has no Name element — makes it non-requestable per WMS spec
    write_text_element(writer, "Title", &config.title);
    write_text_element(writer, "Abstract", &config.description);
    write_keyword_list(writer, &config.keywords);

    // CRS, bbox, time on parent — inherited by children
    write_layer_metadata(writer, info);

    // Attribution (license) after Dimension, before nested child Layers.
    write_attribution(writer, config.license.as_ref());

    // Child layers — one per parameter. When the collection carries a
    // `layer_subtitle` (e.g. a radar site place name), prepend it to the child
    // Title so WMS clients that render a flat layer list (ignoring this parent
    // tree) can still tell sibling collections apart — otherwise every site's
    // child layer is titled identically (just the parameter).
    for (short_name, title) in &info.parameters {
        let child_layer_name = format!("{id}/{short_name}");
        let child_title = match &info.layer_subtitle {
            Some(subtitle) => format!("{subtitle} — {title}"),
            None => title.clone(),
        };
        write_layer(
            writer,
            &child_layer_name,
            config,
            info,
            layer_styles,
            base_url,
            Some(&child_title),
        );
    }

    let _ = writer.write_event(Event::End(BytesEnd::new("Layer")));
}

/// Write a single requestable layer.
/// If `param_title` is Some, this is a child of a multi-param parent and we
/// skip inherited metadata (CRS, bbox, time) since the parent already has it.
fn write_layer(
    writer: &mut Writer<Vec<u8>>,
    layer_name: &str,
    config: &CollectionConfig,
    info: &RasterInfo,
    layer_styles: Option<&HashMap<String, StyleInfo>>,
    base_url: &str,
    param_title: Option<&str>,
) {
    let mut layer = BytesStart::new("Layer");
    layer.push_attribute(("queryable", "0"));
    layer.push_attribute(("opaque", "1"));
    let _ = writer.write_event(Event::Start(layer));

    write_text_element(writer, "Name", layer_name);

    if let Some(title) = param_title {
        write_text_element(writer, "Title", title);
    } else {
        write_text_element(writer, "Title", &config.title);
        write_text_element(writer, "Abstract", &config.description);
        // Keywords belong on the collection's own layer; child layers of a
        // multi-param parent inherit none (the parent carries them).
        write_keyword_list(writer, &config.keywords);
    }

    // For top-level (non-nested) layers, write full metadata.
    // For nested child layers, parent already has CRS/bbox/time.
    if param_title.is_none() {
        write_layer_metadata(writer, info);
        // Attribution (license) after Dimension, before Style.
        write_attribution(writer, config.license.as_ref());
    }

    // Styles
    write_layer_styles(writer, layer_name, layer_styles, base_url);

    let _ = writer.write_event(Event::End(BytesEnd::new("Layer")));
}

/// Write CRS, bbox, and time dimension for a layer.
fn write_layer_metadata(writer: &mut Writer<Vec<u8>>, info: &RasterInfo) {
    // CRS
    for crs in params::supported_crs_list() {
        write_text_element(writer, "CRS", crs);
    }

    // Geographic bounding box
    if let Some([west, south, east, north]) = info.spatial_extent {
        let _ = writer.write_event(Event::Start(BytesStart::new("EX_GeographicBoundingBox")));
        write_text_element(writer, "westBoundLongitude", &format!("{west:.6}"));
        write_text_element(writer, "eastBoundLongitude", &format!("{east:.6}"));
        write_text_element(writer, "southBoundLatitude", &format!("{south:.6}"));
        write_text_element(writer, "northBoundLatitude", &format!("{north:.6}"));
        let _ = writer.write_event(Event::End(BytesEnd::new("EX_GeographicBoundingBox")));

        let mut bb = BytesStart::new("BoundingBox");
        bb.push_attribute(("CRS", "CRS:84"));
        bb.push_attribute(("minx", format!("{west:.6}").as_str()));
        bb.push_attribute(("miny", format!("{south:.6}").as_str()));
        bb.push_attribute(("maxx", format!("{east:.6}").as_str()));
        bb.push_attribute(("maxy", format!("{north:.6}").as_str()));
        let _ = writer.write_event(Event::Empty(bb));
    }

    // Time dimension
    if !info.times.is_empty() {
        let mut dim = BytesStart::new("Dimension");
        dim.push_attribute(("name", "time"));
        dim.push_attribute(("units", "ISO8601"));
        if let Some(latest) = info.times.last() {
            dim.push_attribute(("default", latest.to_rfc3339().as_str()));
        }
        dim.push_attribute(("nearestValue", "1"));
        let _ = writer.write_event(Event::Start(dim));

        let time_values: Vec<String> = info.times.iter().map(|t| t.to_rfc3339()).collect();
        let _ = writer.write_event(Event::Text(BytesText::new(&time_values.join(","))));

        let _ = writer.write_event(Event::End(BytesEnd::new("Dimension")));
    }

    // Elevation dimension — advertised only for layers with a vertical axis.
    if let Some(vertical) = &info.vertical {
        if !vertical.levels.is_empty() {
            let mut dim = BytesStart::new("Dimension");
            dim.push_attribute(("name", "elevation"));
            dim.push_attribute(("units", vertical.unit()));
            let default = format!("{}", vertical.levels[0]);
            dim.push_attribute(("default", default.as_str()));
            dim.push_attribute(("nearestValue", "1"));
            let _ = writer.write_event(Event::Start(dim));

            let level_values: Vec<String> = vertical.levels.iter().map(|v| v.to_string()).collect();
            let _ = writer.write_event(Event::Text(BytesText::new(&level_values.join(","))));

            let _ = writer.write_event(Event::End(BytesEnd::new("Dimension")));
        }
    }

    // Forecast reference-time (model run) dimension — advertised only for
    // forecast collections that retain multiple runs. The standard `time`
    // dimension stays the *valid* time axis; this custom `reference_time`
    // dimension selects the run (the de-facto ncWMS/THREDDS convention,
    // requested as `DIM_REFERENCE_TIME`). Default = latest run. No
    // `nearestValue` — the run must match an advertised value exactly (the
    // handler validates membership and the engine requires an exact match).
    if !info.reference_times.is_empty() {
        let mut dim = BytesStart::new("Dimension");
        dim.push_attribute(("name", "reference_time"));
        dim.push_attribute(("units", "ISO8601"));
        if let Some(latest) = info.reference_times.last() {
            dim.push_attribute(("default", latest.to_rfc3339().as_str()));
        }
        let _ = writer.write_event(Event::Start(dim));

        let run_values: Vec<String> = info
            .reference_times
            .iter()
            .map(|t| t.to_rfc3339())
            .collect();
        let _ = writer.write_event(Event::Text(BytesText::new(&run_values.join(","))));

        let _ = writer.write_event(Event::End(BytesEnd::new("Dimension")));
    }
}

/// Write style elements for a layer.
fn write_layer_styles(
    writer: &mut Writer<Vec<u8>>,
    layer_name: &str,
    layer_styles: Option<&HashMap<String, StyleInfo>>,
    base_url: &str,
) {
    if let Some(styles) = layer_styles {
        let mut style_names: Vec<&String> = styles.keys().collect();
        style_names.sort_by(|a, b| {
            if a.as_str() == "default" {
                std::cmp::Ordering::Less
            } else if b.as_str() == "default" {
                std::cmp::Ordering::Greater
            } else {
                a.cmp(b)
            }
        });
        for name in style_names {
            if let Some(style) = styles.get(name) {
                let _ = writer.write_event(Event::Start(BytesStart::new("Style")));
                write_text_element(writer, "Name", &style.name);
                write_text_element(writer, "Title", &style.title);

                // LegendURL — use the collection ID (before /) for the LAYER param
                let legend_layer = layer_name.split('/').next().unwrap_or(layer_name);
                let _ = writer.write_event(Event::Start(BytesStart::new("LegendURL")));
                let legend_url = format!(
                    "{base_url}/wms?SERVICE=WMS&REQUEST=GetLegendGraphic&LAYER={legend_layer}&STYLE={}&FORMAT=image/png",
                    style.name
                );
                let mut or = BytesStart::new("OnlineResource");
                or.push_attribute(("xlink:type", "simple"));
                or.push_attribute(("xlink:href", legend_url.as_str()));
                let _ = writer.write_event(Event::Empty(or));
                let _ = writer.write_event(Event::End(BytesEnd::new("LegendURL")));

                let _ = writer.write_event(Event::End(BytesEnd::new("Style")));
            }
        }
    } else {
        let _ = writer.write_event(Event::Start(BytesStart::new("Style")));
        write_text_element(writer, "Name", "default");
        write_text_element(writer, "Title", "Default");
        let _ = writer.write_event(Event::End(BytesEnd::new("Style")));
    }
}

/// Write a simple text element using quick-xml (auto-escapes content).
fn write_text_element(writer: &mut Writer<Vec<u8>>, tag: &str, text: &str) {
    let _ = writer.write_event(Event::Start(BytesStart::new(tag)));
    let _ = writer.write_event(Event::Text(BytesText::new(text)));
    let _ = writer.write_event(Event::End(BytesEnd::new(tag)));
}

/// Write a `<KeywordList>` for the collection's configured keywords (nothing when
/// empty). Per the WMS 1.3.0 schema this belongs after `<Abstract>` and before
/// `<CRS>` in a `<Layer>`.
fn write_keyword_list(writer: &mut Writer<Vec<u8>>, keywords: &[String]) {
    if keywords.is_empty() {
        return;
    }
    let _ = writer.write_event(Event::Start(BytesStart::new("KeywordList")));
    for kw in keywords {
        write_text_element(writer, "Keyword", kw);
    }
    let _ = writer.write_event(Event::End(BytesEnd::new("KeywordList")));
}

/// Write an `<Attribution>` carrying the collection's license (nothing when the
/// collection has no license). Per the WMS 1.3.0 schema this belongs after the
/// `<Dimension>` elements and before `<Style>` in a `<Layer>`. The license name
/// is the `<Title>`; a resolvable URL becomes the `<OnlineResource>`.
fn write_attribution(writer: &mut Writer<Vec<u8>>, license: Option<&LicenseConfig>) {
    let Some(license) = license else {
        return;
    };
    let _ = writer.write_event(Event::Start(BytesStart::new("Attribution")));
    write_text_element(writer, "Title", &license.title);
    if let Some(url) = license.resolved_url() {
        let mut or = BytesStart::new("OnlineResource");
        or.push_attribute(("xlink:type", "simple"));
        or.push_attribute(("xlink:href", url.as_str()));
        let _ = writer.write_event(Event::Empty(or));
    }
    let _ = writer.write_event(Event::End(BytesEnd::new("Attribution")));
}
