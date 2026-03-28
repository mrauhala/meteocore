use std::collections::HashMap;
use std::sync::Arc;

use ds_core::config::CollectionConfig;
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
            write_layer(
                &mut writer,
                id,
                config,
                engine.raster_info(),
                layer_styles,
                base_url,
            );
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

    // GetMap
    let _ = writer.write_event(Event::Start(BytesStart::new("GetMap")));
    write_format(writer, "image/png");
    write_format(writer, "image/jpeg");
    write_dcp_type(writer, base_url);
    let _ = writer.write_event(Event::End(BytesEnd::new("GetMap")));

    // GetLegendGraphic
    let _ = writer.write_event(Event::Start(BytesStart::new("GetLegendGraphic")));
    write_format(writer, "image/png");
    write_format(writer, "image/jpeg");
    write_dcp_type(writer, base_url);
    let _ = writer.write_event(Event::End(BytesEnd::new("GetLegendGraphic")));

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
    or.push_attribute(("xmlns:xlink", "http://www.w3.org/1999/xlink"));
    or.push_attribute(("xlink:type", "simple"));
    or.push_attribute(("xlink:href", wms_url.as_str()));
    let _ = writer.write_event(Event::Empty(or));

    let _ = writer.write_event(Event::End(BytesEnd::new("Get")));
    let _ = writer.write_event(Event::End(BytesEnd::new("HTTP")));
    let _ = writer.write_event(Event::End(BytesEnd::new("DCPType")));
}

fn write_layer(
    writer: &mut Writer<Vec<u8>>,
    id: &str,
    config: &CollectionConfig,
    info: RasterInfo,
    layer_styles: Option<&HashMap<String, StyleInfo>>,
    base_url: &str,
) {
    let mut layer = BytesStart::new("Layer");
    layer.push_attribute(("queryable", "0"));
    layer.push_attribute(("opaque", "1"));
    let _ = writer.write_event(Event::Start(layer));

    write_text_element(writer, "Name", id);
    write_text_element(writer, "Title", &config.title);
    write_text_element(writer, "Abstract", &config.description);

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

        // BoundingBox for CRS:84
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

    // Styles
    if let Some(styles) = layer_styles {
        // Sort to ensure "default" comes first
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

                // LegendURL
                let _ = writer.write_event(Event::Start(BytesStart::new("LegendURL")));
                let legend_url = format!(
                    "{base_url}/wms?SERVICE=WMS&REQUEST=GetLegendGraphic&LAYER={id}&STYLE={}&FORMAT=image/png",
                    style.name
                );
                let mut or = BytesStart::new("OnlineResource");
                or.push_attribute(("xmlns:xlink", "http://www.w3.org/1999/xlink"));
                or.push_attribute(("xlink:type", "simple"));
                or.push_attribute(("xlink:href", legend_url.as_str()));
                let _ = writer.write_event(Event::Empty(or));
                let _ = writer.write_event(Event::End(BytesEnd::new("LegendURL")));

                let _ = writer.write_event(Event::End(BytesEnd::new("Style")));
            }
        }
    } else {
        // Fallback if no styles configured
        let _ = writer.write_event(Event::Start(BytesStart::new("Style")));
        write_text_element(writer, "Name", "default");
        write_text_element(writer, "Title", "Default");
        let _ = writer.write_event(Event::End(BytesEnd::new("Style")));
    }

    let _ = writer.write_event(Event::End(BytesEnd::new("Layer")));
}

/// Write a simple text element using quick-xml (auto-escapes content).
fn write_text_element(writer: &mut Writer<Vec<u8>>, tag: &str, text: &str) {
    let _ = writer.write_event(Event::Start(BytesStart::new(tag)));
    let _ = writer.write_event(Event::Text(BytesText::new(text)));
    let _ = writer.write_event(Event::End(BytesEnd::new(tag)));
}
