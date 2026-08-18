//! OGC SLD `RasterSymbolizer/ColorMap` → [`ds_render::Palette`].
//!
//! Lets a deployment reuse the SLD files it already ships to GeoServer/
//! MapServer as MeteoCore colormaps. Parsing is a small streaming pass over
//! `quick-xml` events matching **local** element names, so the sld:/se:/
//! default-namespace spellings of the same document all parse identically
//! (the same approach `engine-cap`'s CAP parser takes).
//!
//! Only the SLD 1.0 `ColorMapEntry` form is supported — the SE 1.1
//! `Categorize`/`Interpolate` functions are not. The first `ColorMap`
//! element in the document wins; anything after it is not read.
//!
//! Security: this is XML *reading*. `quick-xml` resolves only the five
//! predefined entities, so a `<!ENTITY xxe SYSTEM "file:///…">` internal
//! subset is never expanded — an escape referencing it fails the attribute
//! parse instead. Nothing here unwraps on document content.

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use ds_render::{parse_hex_color, ColorStop, Interpolation, Palette};

/// Parse the first `ColorMap` in an SLD document into a named palette.
///
/// `name` becomes the palette's registry key. The result has no title, is
/// not normalized, and carries no explicit nodata color — an SLD ColorMap
/// expresses none of those.
pub fn parse_sld_colormap(name: &str, xml: &str) -> Result<Palette, String> {
    let mut reader = Reader::from_str(xml);

    let mut color_map: Option<ColorMap> = None;

    loop {
        let event = reader
            .read_event()
            .map_err(|e| format!("malformed XML at byte {}: {e}", reader.buffer_position()))?;

        match event {
            Event::Start(e) => match color_map.as_mut() {
                Some(cm) => {
                    if is_local(e.local_name().as_ref(), b"ColorMapEntry") {
                        cm.push_entry(&e)?;
                    }
                    cm.depth += 1;
                }
                None => {
                    if is_local(e.local_name().as_ref(), b"ColorMap") {
                        color_map = Some(ColorMap::open(&e)?);
                    }
                }
            },
            Event::Empty(e) => match color_map.as_mut() {
                Some(cm) => {
                    if is_local(e.local_name().as_ref(), b"ColorMapEntry") {
                        cm.push_entry(&e)?;
                    }
                }
                None => {
                    if is_local(e.local_name().as_ref(), b"ColorMap") {
                        // Self-closing <ColorMap/>: opens and closes at once.
                        return ColorMap::open(&e)?.finish(name);
                    }
                }
            },
            Event::End(e) => {
                let closes_color_map = color_map.as_mut().is_some_and(|cm| {
                    if cm.depth == 0 && is_local(e.local_name().as_ref(), b"ColorMap") {
                        true
                    } else {
                        cm.depth = cm.depth.saturating_sub(1);
                        false
                    }
                });
                if closes_color_map {
                    if let Some(cm) = color_map.take() {
                        return cm.finish(name);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    match color_map {
        // Truncated document: the ColorMap never closed, but its entries are
        // all we needed.
        Some(cm) => cm.finish(name),
        None => Err("no ColorMap element found".to_string()),
    }
}

/// A `ColorMap` element being read.
struct ColorMap {
    interpolation: Interpolation,
    stops: Vec<ColorStop>,
    /// Entry counter, for error messages — counts every `ColorMapEntry`
    /// seen, including the one that failed.
    entry_index: usize,
    /// Open-element nesting below the `ColorMap`, so a nested `</…>` is not
    /// mistaken for the `ColorMap`'s own end tag.
    depth: usize,
}

impl ColorMap {
    fn open(e: &BytesStart) -> Result<Self, String> {
        let interpolation = match attribute(e, b"type")?.as_deref().map(str::trim) {
            // SLD 1.0 §11.4.3: "ramp" is the default.
            None | Some("ramp") => Interpolation::Linear,
            Some("intervals") | Some("values") => Interpolation::Step,
            Some(other) => {
                return Err(format!(
                    "unsupported ColorMap type '{other}' (expected ramp, intervals or values)"
                ))
            }
        };
        Ok(Self {
            interpolation,
            stops: Vec::new(),
            entry_index: 0,
            depth: 0,
        })
    }

    fn push_entry(&mut self, e: &BytesStart) -> Result<(), String> {
        let index = self.entry_index;
        self.entry_index += 1;
        let at = |msg: String| format!("ColorMapEntry[{index}]: {msg}");

        let color = attribute(e, b"color")
            .map_err(at)?
            .ok_or_else(|| at("missing required attribute 'color'".to_string()))?;
        let mut color = parse_hex_color(color.trim())
            .map_err(|err| at(format!("invalid color '{color}': {err}")))?;

        let quantity = attribute(e, b"quantity")
            .map_err(at)?
            .ok_or_else(|| at("missing required attribute 'quantity'".to_string()))?;
        let value: f64 = quantity
            .trim()
            .parse()
            .map_err(|_| at(format!("invalid quantity '{quantity}'")))?;
        if !value.is_finite() {
            return Err(at(format!("quantity '{quantity}' is not a finite number")));
        }

        if let Some(opacity) = attribute(e, b"opacity").map_err(at)? {
            let factor: f64 = opacity
                .trim()
                .parse()
                .map_err(|_| at(format!("invalid opacity '{opacity}'")))?;
            if !(0.0..=1.0).contains(&factor) {
                return Err(at(format!("opacity '{opacity}' is outside 0..1")));
            }
            // Scales the color's alpha, which is 255 for the "#RRGGBB" form
            // SLD specifies — so this is the spec's round(opacity * 255).
            color[3] = (factor * f64::from(color[3])).round() as u8;
        }

        self.stops.push(ColorStop { value, color });
        Ok(())
    }

    fn finish(self, name: &str) -> Result<Palette, String> {
        if self.stops.is_empty() {
            return Err(
                "ColorMap has no ColorMapEntry children — only the SLD 1.0 ColorMapEntry \
                 form is supported, not the SE 1.1 Categorize/Interpolate functions"
                    .to_string(),
            );
        }
        Ok(Palette::new(name, self.stops, self.interpolation))
    }
}

/// Compare an element's local name (namespace prefix already stripped by
/// `local_name()`) against an expected name.
fn is_local(local: &[u8], expected: &[u8]) -> bool {
    local == expected
}

/// The value of an unprefixed attribute by local name, if present.
///
/// `xmlns` declarations are skipped: `xmlns:color` would otherwise have the
/// local name `color`.
fn attribute(e: &BytesStart, want: &[u8]) -> Result<Option<String>, String> {
    for attr in e.attributes() {
        let attr = attr.map_err(|err| format!("malformed attribute: {err}"))?;
        let (local, prefix) = attr.key.decompose();
        if attr.key.as_ref() == b"xmlns" || prefix.is_some_and(|p| p.as_ref() == b"xmlns") {
            continue;
        }
        if local.as_ref() == want {
            let name = String::from_utf8_lossy(want);
            // `normalized_value` replaces the deprecated `unescape_value` and
            // additionally applies XML attribute-value normalization (§3.3.3:
            // tabs/newlines collapse to spaces). `Implicit1_0` — we never read
            // the XML declaration here, and 1.0 is the assumed default; only
            // 1.1 normalizes differently.
            let value = attr
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|err| format!("attribute '{name}' has an unusable value: {err}"))?;
            return Ok(Some(value.into_owned()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GeoServer's usual export shape: SLD 1.0, `sld:` prefixes everywhere,
    /// no `type` attribute, entries as empty elements.
    const GEOSERVER_SLD: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<sld:StyledLayerDescriptor xmlns:sld="http://www.opengis.net/sld"
                           xmlns:ogc="http://www.opengis.net/ogc"
                           version="1.0.0">
  <sld:NamedLayer>
    <sld:Name>radar</sld:Name>
    <sld:UserStyle>
      <sld:FeatureTypeStyle>
        <sld:Rule>
          <sld:RasterSymbolizer>
            <sld:Opacity>1.0</sld:Opacity>
            <sld:ColorMap>
              <sld:ColorMapEntry color="#000000" quantity="-32" opacity="0" label="no echo"/>
              <sld:ColorMapEntry color="#0080FF" quantity="5" label="5 dBZ"/>
              <sld:ColorMapEntry color="#00C800" quantity="25" label="25 dBZ"/>
              <sld:ColorMapEntry color="#FF0000" quantity="50" label="50 dBZ"/>
            </sld:ColorMap>
          </sld:RasterSymbolizer>
        </sld:Rule>
      </sld:FeatureTypeStyle>
    </sld:UserStyle>
  </sld:NamedLayer>
</sld:StyledLayerDescriptor>"##;

    #[test]
    fn geoserver_sld_ramp_is_linear_with_pinned_stops() {
        let p = parse_sld_colormap("radar_house", GEOSERVER_SLD).unwrap();

        assert_eq!(p.name, "radar_house");
        assert_eq!(p.title, None);
        assert!(!p.normalized);
        assert_eq!(p.nodata_color, None);
        assert_eq!(p.interpolation, Interpolation::Linear);

        assert_eq!(p.stops.len(), 4);
        assert_eq!(p.stops[0].value, -32.0);
        assert_eq!(p.stops[0].color, [0, 0, 0, 0]);
        assert_eq!(p.stops[1].value, 5.0);
        assert_eq!(p.stops[1].color, [0, 128, 255, 255]);
        assert_eq!(p.stops[2].value, 25.0);
        assert_eq!(p.stops[2].color, [0, 200, 0, 255]);
        assert_eq!(p.stops[3].value, 50.0);
        assert_eq!(p.stops[3].color, [255, 0, 0, 255]);
        assert_eq!(p.default_range(), Some((-32.0, 50.0)));
    }

    #[test]
    fn intervals_and_values_types_are_step() {
        let xml = |t: &str| {
            format!(
                r##"<StyledLayerDescriptor><ColorMap type="{t}">
                     <ColorMapEntry color="#000000" quantity="0"/>
                     <ColorMapEntry color="#FFFFFF" quantity="10"/>
                   </ColorMap></StyledLayerDescriptor>"##
            )
        };
        for t in ["intervals", "values"] {
            let p = parse_sld_colormap("c", &xml(t)).unwrap();
            assert_eq!(p.interpolation, Interpolation::Step, "type={t}");
            assert_eq!(p.stops.len(), 2);
        }
        let ramp = parse_sld_colormap("c", &xml("ramp")).unwrap();
        assert_eq!(ramp.interpolation, Interpolation::Linear);
        assert!(parse_sld_colormap("c", &xml("categorize")).is_err());
    }

    #[test]
    fn opacity_scales_alpha() {
        let xml = r##"<ColorMap>
            <ColorMapEntry color="#112233" quantity="0" opacity="0"/>
            <ColorMapEntry color="#112233" quantity="1" opacity="0.5"/>
            <ColorMapEntry color="#112233" quantity="2" opacity="1.0"/>
            <ColorMapEntry color="#112233" quantity="3"/>
            <ColorMapEntry color="#11223380" quantity="4" opacity="0.5"/>
        </ColorMap>"##;
        let p = parse_sld_colormap("c", xml).unwrap();
        assert_eq!(p.stops[0].color, [17, 34, 51, 0]);
        assert_eq!(p.stops[1].color, [17, 34, 51, 128]);
        assert_eq!(p.stops[2].color, [17, 34, 51, 255]);
        assert_eq!(p.stops[3].color, [17, 34, 51, 255]);
        // "#RRGGBBAA" is outside SLD, but scaling stays sane: 128 * 0.5.
        assert_eq!(p.stops[4].color, [17, 34, 51, 64]);

        assert!(parse_sld_colormap(
            "c",
            r##"<ColorMap><ColorMapEntry color="#112233" quantity="0" opacity="1.5"/></ColorMap>"##
        )
        .is_err());
        assert!(parse_sld_colormap(
            "c",
            r##"<ColorMap><ColorMapEntry color="#112233" quantity="0" opacity="-0.1"/></ColorMap>"##
        )
        .is_err());
        assert!(parse_sld_colormap(
            "c",
            r##"<ColorMap><ColorMapEntry color="#112233" quantity="0" opacity="半"/></ColorMap>"##
        )
        .is_err());
    }

    /// The same document with a default namespace and `se:` prefixes must
    /// parse to exactly the same palette — element matching is on local
    /// names only.
    #[test]
    fn namespace_spellings_agree() {
        let default_ns = r##"<?xml version="1.0" encoding="UTF-8"?>
<StyledLayerDescriptor xmlns="http://www.opengis.net/sld" version="1.0.0">
  <NamedLayer><UserStyle><FeatureTypeStyle><Rule><RasterSymbolizer>
    <ColorMap>
      <ColorMapEntry color="#000000" quantity="-32" opacity="0" label="no echo"/>
      <ColorMapEntry color="#0080FF" quantity="5" label="5 dBZ"/>
      <ColorMapEntry color="#00C800" quantity="25" label="25 dBZ"/>
      <ColorMapEntry color="#FF0000" quantity="50" label="50 dBZ"/>
    </ColorMap>
  </RasterSymbolizer></Rule></FeatureTypeStyle></UserStyle></NamedLayer>
</StyledLayerDescriptor>"##;
        let se_ns = r##"<sld:StyledLayerDescriptor xmlns:sld="http://www.opengis.net/sld"
                                                  xmlns:se="http://www.opengis.net/se">
  <se:RasterSymbolizer><se:ColorMap>
    <se:ColorMapEntry color="#000000" quantity="-32" opacity="0"/>
    <se:ColorMapEntry color="#0080FF" quantity="5"/>
    <se:ColorMapEntry color="#00C800" quantity="25"/>
    <se:ColorMapEntry color="#FF0000" quantity="50"/>
  </se:ColorMap></se:RasterSymbolizer>
</sld:StyledLayerDescriptor>"##;

        let reference = parse_sld_colormap("c", GEOSERVER_SLD).unwrap();
        for xml in [default_ns, se_ns] {
            let p = parse_sld_colormap("c", xml).unwrap();
            assert_eq!(p.interpolation, reference.interpolation);
            assert_eq!(p.stops.len(), reference.stops.len());
            for (got, want) in p.stops.iter().zip(&reference.stops) {
                assert_eq!(got.value, want.value);
                assert_eq!(got.color, want.color);
            }
        }
    }

    #[test]
    fn descending_entries_come_out_ascending() {
        let xml = r##"<ColorMap>
            <ColorMapEntry color="#FF0000" quantity="50"/>
            <ColorMapEntry color="#00FF00" quantity="25"/>
            <ColorMapEntry color="#0000FF" quantity="-5.5"/>
        </ColorMap>"##;
        let p = parse_sld_colormap("c", xml).unwrap();
        let values: Vec<f64> = p.stops.iter().map(|s| s.value).collect();
        assert_eq!(values, vec![-5.5, 25.0, 50.0]);
        assert_eq!(p.stops[0].color, [0, 0, 255, 255]);
        assert_eq!(p.stops[2].color, [255, 0, 0, 255]);
    }

    /// Only the first ColorMap is read; entries of later ones are ignored.
    #[test]
    fn first_color_map_wins() {
        let xml = r##"<StyledLayerDescriptor>
          <RasterSymbolizer><ColorMap type="intervals">
            <ColorMapEntry color="#000000" quantity="0"/>
            <ColorMapEntry color="#FFFFFF" quantity="1"/>
          </ColorMap></RasterSymbolizer>
          <RasterSymbolizer><ColorMap>
            <ColorMapEntry color="#FF0000" quantity="99"/>
          </ColorMap></RasterSymbolizer>
        </StyledLayerDescriptor>"##;
        let p = parse_sld_colormap("c", xml).unwrap();
        assert_eq!(p.interpolation, Interpolation::Step);
        assert_eq!(p.stops.len(), 2);
        assert_eq!(p.stops.last().unwrap().value, 1.0);
    }

    /// A ColorMapEntry written as a Start/End pair rather than an empty
    /// element still counts, and its nested end tag doesn't close the map.
    #[test]
    fn non_empty_entry_elements_parse() {
        let xml = r##"<ColorMap>
            <ColorMapEntry color="#000000" quantity="0"></ColorMapEntry>
            <ColorMapEntry color="#FFFFFF" quantity="1"></ColorMapEntry>
        </ColorMap>"##;
        let p = parse_sld_colormap("c", xml).unwrap();
        assert_eq!(p.stops.len(), 2);
    }

    #[test]
    fn rejects_documents_without_usable_entries() {
        let no_color_map = r##"<StyledLayerDescriptor><NamedLayer><Name>x</Name></NamedLayer></StyledLayerDescriptor>"##;
        assert_eq!(
            parse_sld_colormap("c", no_color_map).unwrap_err(),
            "no ColorMap element found"
        );
        assert!(parse_sld_colormap("c", "").is_err());

        // SE 1.1 Categorize: a ColorMap with no ColorMapEntry children.
        let categorize = r##"<ColorMap><Categorize fallbackValue="#000000">
            <LookupValue>Rasterdata</LookupValue>
            <Value>#00FF00</Value><Threshold>25</Threshold>
        </Categorize></ColorMap>"##;
        let err = parse_sld_colormap("c", categorize).unwrap_err();
        assert!(err.contains("ColorMapEntry"), "{err}");
        assert!(parse_sld_colormap("c", "<ColorMap/>").is_err());
    }

    #[test]
    fn rejects_bad_entries_naming_the_index_and_attribute() {
        let missing_quantity = r##"<ColorMap>
            <ColorMapEntry color="#000000" quantity="0"/>
            <ColorMapEntry color="#FFFFFF"/>
        </ColorMap>"##;
        let err = parse_sld_colormap("c", missing_quantity).unwrap_err();
        assert!(err.contains("ColorMapEntry[1]"), "{err}");
        assert!(err.contains("quantity"), "{err}");

        let bad_quantity =
            r##"<ColorMap><ColorMapEntry color="#000000" quantity="lots"/></ColorMap>"##;
        let err = parse_sld_colormap("c", bad_quantity).unwrap_err();
        assert!(err.contains("ColorMapEntry[0]"), "{err}");
        assert!(err.contains("lots"), "{err}");

        // NaN/inf parse as f64 but would poison stop ordering.
        for q in ["NaN", "inf"] {
            let xml = format!(
                r##"<ColorMap><ColorMapEntry color="#000000" quantity="{q}"/></ColorMap>"##
            );
            assert!(parse_sld_colormap("c", &xml).is_err(), "quantity={q}");
        }

        let bad_color = r##"<ColorMap><ColorMapEntry color="#GGG" quantity="0"/></ColorMap>"##;
        let err = parse_sld_colormap("c", bad_color).unwrap_err();
        assert!(err.contains("ColorMapEntry[0]"), "{err}");
        assert!(err.contains("color"), "{err}");

        let missing_color = r##"<ColorMap><ColorMapEntry quantity="0"/></ColorMap>"##;
        assert!(parse_sld_colormap("c", missing_color)
            .unwrap_err()
            .contains("color"));
    }

    #[test]
    fn malformed_xml_errors_without_panicking() {
        for xml in [
            r##"<ColorMap><ColorMapEntry color="#000000" quantity="0"/>"##, // unclosed
            r##"<ColorMap><ColorMapEntry color="#000000" quantity="0"/></NotColorMap>"##,
            r##"<ColorMap><ColorMapEntry color=#000000 quantity="0"/></ColorMap>"##, // unquoted
            r##"<ColorMap><ColorMapEntry color="#000000" color="#FFFFFF" quantity="0"/></ColorMap>"##,
            "<<<>>>",
            "\u{feff}\u{0}<ColorMap",
        ] {
            // Only "must not panic" is pinned here; some of these are
            // recoverable enough that quick-xml still yields the entries.
            let _ = parse_sld_colormap("c", xml);
        }

        // The unquoted-attribute and duplicate-attribute cases must be
        // rejected rather than silently mis-parsed.
        assert!(parse_sld_colormap(
            "c",
            r##"<ColorMap><ColorMapEntry color=#000000 quantity="0"/></ColorMap>"##
        )
        .is_err());
        assert!(parse_sld_colormap(
            "c",
            r##"<ColorMap><ColorMapEntry color="#000000" color="#FFFFFF" quantity="0"/></ColorMap>"##
        )
        .is_err());
    }

    /// An internal DTD subset declaring an external entity must never be
    /// expanded (XXE). Referencing it in an ignored attribute is harmless;
    /// referencing it in one we read must fail, not resolve.
    #[test]
    fn doctype_entity_is_not_resolved() {
        let in_label = r##"<?xml version="1.0"?>
<!DOCTYPE StyledLayerDescriptor [
  <!ENTITY xxe SYSTEM "file:///etc/passwd">
]>
<StyledLayerDescriptor><RasterSymbolizer><ColorMap>
  <ColorMapEntry color="#000000" quantity="0" label="&xxe;"/>
  <ColorMapEntry color="#FFFFFF" quantity="1"/>
</ColorMap></RasterSymbolizer></StyledLayerDescriptor>"##;
        // Either outcome is acceptable; what matters is no panic and no
        // filesystem read. `label` is never unescaped, so this parses.
        if let Ok(p) = parse_sld_colormap("c", in_label) {
            assert_eq!(p.stops.len(), 2);
            assert_eq!(p.stops[0].color, [0, 0, 0, 255]);
        }

        let in_color = r##"<?xml version="1.0"?>
<!DOCTYPE ColorMap [ <!ENTITY xxe SYSTEM "file:///etc/passwd"> ]>
<ColorMap><ColorMapEntry color="&xxe;" quantity="0"/></ColorMap>"##;
        let err = parse_sld_colormap("c", in_color).unwrap_err();
        assert!(!err.contains("root:"), "entity was expanded: {err}");
    }
}
