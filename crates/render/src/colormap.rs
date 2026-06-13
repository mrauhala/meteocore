/// Trait for mapping raster values to RGBA colors.
pub trait ColorMap: Send + Sync {
    /// Map a value to an RGBA color. None = nodata → transparent.
    fn color(&self, value: Option<f64>) -> [u8; 4];

    /// Colour returned for `None` / NaN / ±∞ inputs. Defaults to fully transparent.
    /// Overridden by concrete impls that carry an explicit nodata colour so
    /// wrappers like `IntegerLutColorMap::from_colormap` can preserve it.
    fn nodata_color(&self) -> [u8; 4] {
        [0, 0, 0, 0]
    }
}

/// A color stop for defining gradient colormaps.
pub struct ColorStop {
    pub value: f64,
    pub color: [u8; 4],
}

/// Built-in colormap names.
pub enum BuiltinColormap {
    /// Standard radar reflectivity (blue → green → yellow → red).
    RadarDbz,
    /// SMHI radar reflectivity with gray below-threshold, per-dBZ colors, -29 to 70 dBZ.
    RadarSmhi,
    /// FMI summer radar reflectivity (cyan → green → yellow → orange → red → magenta → pink).
    RadarFmi,
    /// Bookbinder 8-bit Z curve (Evan Bookbinder, WFO SGF). Full dBZ range to 95.
    RadarBookbinder,
    /// Linear grayscale (black → white).
    Grayscale,
    /// Perceptually uniform (dark purple → blue → green → yellow).
    Viridis,
    /// Temperature palette (blue → cyan → green → yellow → red).
    Temperature,
    /// Precipitation accumulation palette (light blue → blue → purple → magenta). 0–50 mm.
    Precipitation,
    /// Precipitation rate palette (transparent → cyan → blue → green → yellow → red). 0–30 mm/h.
    PrecipitationRate,
    /// Wind speed palette (calm green → yellow → orange → red ��� purple).
    WindSpeed,
}

/// Lookup-table colormap. O(1) per pixel.
/// Best for integer-like data (radar U8, classification).
pub struct LutColorMap {
    lut: Vec<[u8; 4]>,
    min: f64,
    max: f64,
    nodata_color: [u8; 4],
}

impl LutColorMap {
    /// Create a LUT colormap from a built-in palette.
    ///
    /// The min/max define the value range mapped to the LUT.
    /// Values in the stops define which colors appear at which physical values.
    /// The LUT samples the stops across the min..max range.
    pub fn from_builtin(builtin: BuiltinColormap, min: f64, max: f64) -> Self {
        let stops = builtin_stops(&builtin);
        Self::from_stops(&stops, min, max)
    }

    /// Pre-compute an integer LUT from this colormap over `[min_val, max_val]`.
    ///
    /// Returns `None` if the range exceeds 65,536 entries.
    pub fn to_integer_lut(&self, min_val: i64, max_val: i64) -> Option<IntegerLutColorMap> {
        IntegerLutColorMap::from_colormap(self, min_val, max_val)
    }

    /// Create a LUT colormap from color stops.
    ///
    /// The `min`/`max` define the data value range. The color stops define
    /// which colors appear at which values within (or beyond) that range.
    /// The LUT has 4096 entries for better precision with wide value ranges.
    pub fn from_stops(stops: &[ColorStop], min: f64, max: f64) -> Self {
        let lut_size = 4096;
        let mut lut = Vec::with_capacity(lut_size);

        for i in 0..lut_size {
            let t = i as f64 / (lut_size - 1) as f64;
            let value = min + t * (max - min);
            lut.push(interpolate_stops(stops, value));
        }

        Self {
            lut,
            min,
            max,
            nodata_color: [0, 0, 0, 0],
        }
    }
}

impl ColorMap for LutColorMap {
    fn nodata_color(&self) -> [u8; 4] {
        self.nodata_color
    }

    fn color(&self, value: Option<f64>) -> [u8; 4] {
        match value {
            None => self.nodata_color,
            // NaN/±∞ are not real data — colour them as nodata. Deliberate
            // behaviour change: before #250, NaN here hit the saturating cast
            // `NaN as usize = 0` and returned `lut[0]` (opaque for built-ins
            // like Viridis/Temperature); `LinearColorMap` returned the LAST
            // stop. The two were quietly inconsistent. This guard unifies all
            // three colormap impls on `nodata_color`, which is what a NaN
            // pixel actually means.
            Some(v) if !v.is_finite() => self.nodata_color,
            Some(v) => {
                if self.max <= self.min {
                    return self.lut[0];
                }
                let t = (v - self.min) / (self.max - self.min);
                let idx = (t * (self.lut.len() - 1) as f64)
                    .round()
                    .clamp(0.0, (self.lut.len() - 1) as f64) as usize;
                self.lut[idx]
            }
        }
    }
}

/// Pre-computed integer LUT colormap. O(1) per pixel with direct indexing.
///
/// All colors over the configured integer range are pre-computed into a flat
/// array. Per-pixel lookup is one subtract + one `.round()` + clamp + index —
/// matching [`LutColorMap`]'s round-nearest + saturate-clamp semantics at
/// integer entries, while replacing its `(v - min) / (max - min) * (len - 1)`
/// normalisation with a single offset shift.
///
/// Maximum supported range: 65,536 entries (~256 KB for UInt16/Int16).
pub struct IntegerLutColorMap {
    lut: Vec<[u8; 4]>,
    offset: i64,
    nodata_color: [u8; 4],
}

/// Maximum number of entries in an integer LUT (covers full UInt16 / Int16 range).
const MAX_INTEGER_LUT_SIZE: usize = 65_536;

impl IntegerLutColorMap {
    /// Pre-compute a LUT for integer data in the range `[min_val, max_val]` (inclusive).
    ///
    /// Each integer value in the range gets its color computed once from the
    /// source colormap. At render time, lookup is `O(1)` per pixel.
    ///
    /// Returns `None` if the range exceeds 65,536 entries.
    pub fn from_colormap(source: &dyn ColorMap, min_val: i64, max_val: i64) -> Option<Self> {
        let nodata_color = source.nodata_color();
        if max_val < min_val {
            return Some(Self {
                lut: Vec::new(),
                offset: 0,
                nodata_color,
            });
        }
        let range = (max_val - min_val + 1) as usize;
        if range > MAX_INTEGER_LUT_SIZE {
            return None;
        }
        let mut lut = Vec::with_capacity(range);
        for i in 0..range {
            let value = (i as i64 + min_val) as f64;
            lut.push(source.color(Some(value)));
        }
        Some(Self {
            lut,
            offset: min_val,
            nodata_color,
        })
    }
}

impl ColorMap for IntegerLutColorMap {
    fn nodata_color(&self) -> [u8; 4] {
        self.nodata_color
    }

    fn color(&self, value: Option<f64>) -> [u8; 4] {
        // Round-nearest + saturate-clamp, matching `LutColorMap`/`LinearColorMap`
        // semantics. `v as i64` would truncate toward zero (23.6 → 23, -0.7 → 0)
        // which diverges from the float path's `.round()` (24 and -1). Out-of-range
        // values clamp to the first/last entry rather than fall to transparent,
        // also matching the float path; transparent here would silently hide
        // pixels just outside the configured range.
        match value {
            None => self.nodata_color,
            Some(v) if !v.is_finite() => self.nodata_color,
            Some(v) => {
                if self.lut.is_empty() {
                    return self.nodata_color;
                }
                let last = (self.lut.len() - 1) as i64;
                let idx = (v - self.offset as f64).round() as i64;
                self.lut[idx.clamp(0, last) as usize]
            }
        }
    }
}

/// Wraps an inner colormap so a single reserved **sentinel** value renders as
/// one fixed colour, while every other value (and nodata) delegates to the
/// inner colormap.
///
/// This is how a single colormap-driven raster layer carries two visually
/// distinct symbol classes: the derived storm-cell (`CELLS`) overlay paints
/// cell outlines/markers at their dBZ value (→ the inner radar ramp) and
/// track trails at the sentinel (→ one neutral colour), so trails read as
/// subordinate to the intensity-coloured cells instead of blending into them.
/// The sentinel is matched by exact `f64` equality, so the producer must
/// paint trails at the identical constant (`==`, no rounding).
pub struct OverlayColorMap {
    inner: std::sync::Arc<dyn ColorMap>,
    sentinel: f64,
    overlay_color: [u8; 4],
}

impl OverlayColorMap {
    pub fn new(inner: std::sync::Arc<dyn ColorMap>, sentinel: f64, overlay_color: [u8; 4]) -> Self {
        // A NaN sentinel could never match (`NaN != NaN`), silently making this
        // a pure passthrough — catch that misuse in debug builds.
        debug_assert!(
            sentinel.is_finite(),
            "OverlayColorMap sentinel must be finite; NaN/±inf never match"
        );
        Self {
            inner,
            sentinel,
            overlay_color,
        }
    }
}

impl ColorMap for OverlayColorMap {
    fn nodata_color(&self) -> [u8; 4] {
        self.inner.nodata_color()
    }

    fn color(&self, value: Option<f64>) -> [u8; 4] {
        match value {
            // Exact match (the producer paints this literal constant), and
            // finite so it never collides with the nodata/NaN path.
            Some(v) if v == self.sentinel => self.overlay_color,
            other => self.inner.color(other),
        }
    }
}

/// Linear interpolation colormap for continuous data.
pub struct LinearColorMap {
    stops: Vec<ColorStop>,
    nodata_color: [u8; 4],
}

impl LinearColorMap {
    pub fn new(stops: Vec<ColorStop>) -> Self {
        Self {
            stops,
            nodata_color: [0, 0, 0, 0],
        }
    }

    /// Pre-compute an integer LUT for this colormap over `[min_val, max_val]`.
    ///
    /// Returns `None` if the range exceeds 65,536 entries.
    pub fn to_integer_lut(&self, min_val: i64, max_val: i64) -> Option<IntegerLutColorMap> {
        IntegerLutColorMap::from_colormap(self, min_val, max_val)
    }
}

impl ColorMap for LinearColorMap {
    fn nodata_color(&self) -> [u8; 4] {
        self.nodata_color
    }

    fn color(&self, value: Option<f64>) -> [u8; 4] {
        match value {
            None => self.nodata_color,
            // Match the NaN guard in LutColorMap / IntegerLutColorMap — otherwise
            // a NaN falls through every comparison in `interpolate_stops` and
            // returns the LAST stop's colour, diverging from the other paths.
            Some(v) if !v.is_finite() => self.nodata_color,
            Some(v) => interpolate_stops(&self.stops, v),
        }
    }
}

/// Interpolate between color stops to find the color for a given value.
fn interpolate_stops(stops: &[ColorStop], value: f64) -> [u8; 4] {
    if stops.is_empty() {
        return [0, 0, 0, 255];
    }
    if stops.len() == 1 || value <= stops[0].value {
        return stops[0].color;
    }
    if value >= stops[stops.len() - 1].value {
        return stops[stops.len() - 1].color;
    }

    // Find the two surrounding stops
    for i in 0..stops.len() - 1 {
        let lo = &stops[i];
        let hi = &stops[i + 1];
        if value >= lo.value && value <= hi.value {
            let range = hi.value - lo.value;
            if range == 0.0 {
                return lo.color;
            }
            let t = (value - lo.value) / range;
            return [
                lerp_u8(lo.color[0], hi.color[0], t),
                lerp_u8(lo.color[1], hi.color[1], t),
                lerp_u8(lo.color[2], hi.color[2], t),
                lerp_u8(lo.color[3], hi.color[3], t),
            ];
        }
    }

    stops[stops.len() - 1].color
}

fn lerp_u8(a: u8, b: u8, t: f64) -> u8 {
    (a as f64 + (b as f64 - a as f64) * t).round() as u8
}

/// Parse a hex color string like "#RRGGBB" or "#RRGGBBAA".
pub fn parse_hex_color(s: &str) -> Result<[u8; 4], String> {
    let s = s.strip_prefix('#').unwrap_or(s);
    match s.len() {
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).map_err(|e| e.to_string())?;
            let g = u8::from_str_radix(&s[2..4], 16).map_err(|e| e.to_string())?;
            let b = u8::from_str_radix(&s[4..6], 16).map_err(|e| e.to_string())?;
            Ok([r, g, b, 255])
        }
        8 => {
            let r = u8::from_str_radix(&s[0..2], 16).map_err(|e| e.to_string())?;
            let g = u8::from_str_radix(&s[2..4], 16).map_err(|e| e.to_string())?;
            let b = u8::from_str_radix(&s[4..6], 16).map_err(|e| e.to_string())?;
            let a = u8::from_str_radix(&s[6..8], 16).map_err(|e| e.to_string())?;
            Ok([r, g, b, a])
        }
        _ => Err(format!(
            "invalid hex color: '{s}' (expected 6 or 8 hex digits)"
        )),
    }
}

/// Get built-in color stops for a named colormap.
pub fn builtin_stops(builtin: &BuiltinColormap) -> Vec<ColorStop> {
    match builtin {
        BuiltinColormap::Grayscale => vec![
            ColorStop {
                value: 0.0,
                color: [0, 0, 0, 255],
            },
            ColorStop {
                value: 1.0,
                color: [255, 255, 255, 255],
            },
        ],
        BuiltinColormap::RadarDbz => vec![
            ColorStop {
                value: 0.0,
                color: [0, 0, 0, 0],
            }, // transparent (no echo)
            ColorStop {
                value: 5.0,
                color: [0, 0, 0, 0],
            }, // transparent (below threshold)
            ColorStop {
                value: 5.1,
                color: [0, 128, 255, 255],
            }, // light blue
            ColorStop {
                value: 15.0,
                color: [0, 200, 255, 255],
            }, // cyan
            ColorStop {
                value: 25.0,
                color: [0, 200, 0, 255],
            }, // green
            ColorStop {
                value: 30.0,
                color: [0, 255, 0, 255],
            }, // bright green
            ColorStop {
                value: 35.0,
                color: [255, 255, 0, 255],
            }, // yellow
            ColorStop {
                value: 40.0,
                color: [255, 200, 0, 255],
            }, // orange-yellow
            ColorStop {
                value: 45.0,
                color: [255, 128, 0, 255],
            }, // orange
            ColorStop {
                value: 50.0,
                color: [255, 0, 0, 255],
            }, // red
            ColorStop {
                value: 55.0,
                color: [200, 0, 0, 255],
            }, // dark red
            ColorStop {
                value: 60.0,
                color: [180, 0, 180, 255],
            }, // magenta
            ColorStop {
                value: 70.0,
                color: [255, 255, 255, 255],
            }, // white (extreme)
        ],
        BuiltinColormap::RadarSmhi => {
            // SMHI radar reflectivity colormap with per-dBZ colors.
            // Gray tones below 5 dBZ, then blue → green → yellow → orange → red → magenta → cyan.
            let data: &[(f64, [u8; 4])] = &[
                (-30.0, [0, 0, 0, 0]), // below range: transparent
                (-29.1, [0, 0, 0, 0]),
                (-29.0, [54, 54, 54, 255]), // gray ramp starts
                (-20.0, [63, 63, 63, 255]),
                (-10.0, [73, 73, 73, 255]),
                (-6.0, [87, 87, 87, 255]),
                (-1.0, [139, 139, 139, 255]),
                (0.0, [150, 150, 150, 255]),
                (4.0, [192, 192, 192, 255]),
                (5.0, [0, 50, 255, 255]), // blue ramp
                (8.0, [0, 110, 255, 255]),
                (11.0, [0, 170, 255, 255]),
                (12.0, [0, 128, 0, 255]), // green ramp
                (15.0, [0, 163, 0, 255]),
                (19.0, [0, 178, 0, 255]),
                (20.0, [10, 208, 10, 255]), // bright green
                (24.0, [10, 248, 10, 255]),
                (25.0, [255, 255, 15, 255]), // yellow ramp
                (29.0, [255, 220, 15, 255]),
                (30.0, [255, 200, 0, 255]), // orange ramp
                (34.0, [255, 120, 0, 255]),
                (35.0, [255, 35, 35, 255]), // red ramp
                (37.0, [255, 0, 0, 255]),
                (40.0, [195, 0, 0, 255]),
                (44.0, [115, 0, 0, 255]),
                (45.0, [175, 0, 175, 255]), // magenta ramp
                (50.0, [219, 0, 219, 255]),
                (54.0, [255, 0, 255, 255]),
                (55.0, [0, 255, 255, 255]), // cyan ramp
                (60.0, [64, 255, 255, 255]),
                (65.0, [128, 255, 255, 255]),
                (70.0, [192, 255, 255, 255]),
            ];
            data.iter()
                .map(|(v, c)| ColorStop {
                    value: *v,
                    color: *c,
                })
                .collect()
        }
        BuiltinColormap::RadarFmi => {
            // FMI summer radar reflectivity. Converted from SLD raw values
            // to dBZ via: dBZ = raw * 0.5 - 32. Below 5 dBZ is transparent.
            let data: &[(f64, [u8; 4])] = &[
                (-32.0, [0, 0, 0, 0]),       // below range: transparent
                (5.0, [0, 0, 0, 0]),         // raw 74: below threshold
                (8.0, [108, 235, 243, 255]), // raw 80: light cyan
                (12.0, [88, 199, 151, 255]), // raw 88: green
                (18.0, [64, 152, 87, 255]),  // raw 100: dark green
                (24.0, [241, 243, 90, 255]), // raw 112: yellow
                (30.0, [223, 196, 10, 255]), // raw 124: gold
                (34.0, [235, 149, 26, 255]), // raw 132: orange
                (40.0, [232, 86, 22, 255]),  // raw 144: red-orange
                (46.0, [206, 2, 2, 255]),    // raw 156: red
                (52.0, [131, 10, 70, 255]),  // raw 168: dark magenta
                (58.0, [250, 81, 165, 255]), // raw 180: pink
            ];
            data.iter()
                .map(|(v, c)| ColorStop {
                    value: *v,
                    color: *c,
                })
                .collect()
        }
        BuiltinColormap::RadarBookbinder => {
            // Bookbinder 8-bit Z curve (Evan Bookbinder, WFO SGF).
            // Converted from SLD raw values to dBZ via: dBZ = raw * 0.5 - 32.
            let data: &[(f64, [u8; 4])] = &[
                (-32.0, [0, 0, 0, 0]),        // raw 0: transparent
                (-31.5, [96, 96, 96, 77]),    // raw 1: faint gray
                (4.5, [96, 96, 96, 77]),      // raw 73: gray
                (5.0, [32, 96, 128, 179]),    // raw 74: dark cyan
                (19.5, [48, 208, 255, 255]),  // raw 103: bright cyan
                (20.0, [0, 255, 0, 255]),     // raw 104: bright green
                (39.5, [0, 76, 0, 255]),      // raw 143: dark green
                (40.0, [255, 230, 0, 255]),   // raw 144: yellow
                (49.5, [255, 128, 0, 255]),   // raw 163: orange
                (50.0, [255, 0, 0, 255]),     // raw 164: red
                (59.5, [96, 0, 0, 255]),      // raw 183: dark red
                (60.0, [255, 255, 255, 255]), // raw 184: white
                (64.5, [255, 255, 255, 255]), // raw 193: white
                (65.0, [144, 48, 208, 255]),  // raw 194: purple
                (69.5, [144, 48, 208, 255]),  // raw 203: purple
                (70.0, [255, 32, 255, 255]),  // raw 204: magenta
                (74.5, [255, 32, 255, 255]),  // raw 213: magenta
                (75.0, [255, 0, 128, 255]),   // raw 214: hot pink
                (79.5, [255, 0, 128, 255]),   // raw 223: hot pink
                (80.0, [255, 0, 150, 255]),   // raw 224: deep pink
                (94.5, [255, 0, 150, 255]),   // raw 253: deep pink
            ];
            data.iter()
                .map(|(v, c)| ColorStop {
                    value: *v,
                    color: *c,
                })
                .collect()
        }
        BuiltinColormap::Viridis => vec![
            ColorStop {
                value: 0.0,
                color: [68, 1, 84, 255],
            },
            ColorStop {
                value: 0.125,
                color: [72, 36, 117, 255],
            },
            ColorStop {
                value: 0.25,
                color: [56, 88, 140, 255],
            },
            ColorStop {
                value: 0.375,
                color: [38, 130, 142, 255],
            },
            ColorStop {
                value: 0.5,
                color: [31, 158, 137, 255],
            },
            ColorStop {
                value: 0.625,
                color: [78, 178, 101, 255],
            },
            ColorStop {
                value: 0.75,
                color: [148, 197, 56, 255],
            },
            ColorStop {
                value: 0.875,
                color: [220, 215, 30, 255],
            },
            ColorStop {
                value: 1.0,
                color: [253, 231, 37, 255],
            },
        ],
        BuiltinColormap::Temperature => vec![
            ColorStop {
                value: -40.0,
                color: [40, 0, 120, 255],
            }, // deep purple (extreme cold)
            ColorStop {
                value: -30.0,
                color: [0, 0, 180, 255],
            }, // dark blue
            ColorStop {
                value: -20.0,
                color: [0, 60, 255, 255],
            }, // blue
            ColorStop {
                value: -10.0,
                color: [0, 160, 255, 255],
            }, // light blue
            ColorStop {
                value: 0.0,
                color: [0, 220, 220, 255],
            }, // cyan
            ColorStop {
                value: 10.0,
                color: [0, 200, 0, 255],
            }, // green
            ColorStop {
                value: 20.0,
                color: [200, 200, 0, 255],
            }, // yellow
            ColorStop {
                value: 30.0,
                color: [255, 128, 0, 255],
            }, // orange
            ColorStop {
                value: 40.0,
                color: [255, 0, 0, 255],
            }, // red
            ColorStop {
                value: 50.0,
                color: [180, 0, 0, 255],
            }, // dark red
        ],
        BuiltinColormap::Precipitation => vec![
            ColorStop {
                value: 0.0,
                color: [0, 0, 0, 0],
            }, // transparent (no precip)
            ColorStop {
                value: 0.1,
                color: [170, 220, 255, 255],
            }, // very light blue
            ColorStop {
                value: 0.5,
                color: [100, 180, 255, 255],
            }, // light blue
            ColorStop {
                value: 1.0,
                color: [50, 130, 255, 255],
            }, // blue
            ColorStop {
                value: 2.0,
                color: [0, 80, 255, 255],
            }, // medium blue
            ColorStop {
                value: 5.0,
                color: [0, 40, 200, 255],
            }, // dark blue
            ColorStop {
                value: 10.0,
                color: [120, 0, 200, 255],
            }, // purple
            ColorStop {
                value: 20.0,
                color: [200, 0, 150, 255],
            }, // magenta
            ColorStop {
                value: 50.0,
                color: [255, 255, 255, 255],
            }, // white (extreme)
        ],
        BuiltinColormap::PrecipitationRate => vec![
            ColorStop {
                value: 0.0,
                color: [0, 0, 0, 0],
            }, // transparent (no rain)
            ColorStop {
                value: 0.1,
                color: [200, 240, 255, 200],
            }, // very light cyan (drizzle)
            ColorStop {
                value: 0.5,
                color: [100, 210, 255, 255],
            }, // light cyan
            ColorStop {
                value: 1.0,
                color: [30, 170, 255, 255],
            }, // cyan-blue
            ColorStop {
                value: 2.0,
                color: [0, 120, 200, 255],
            }, // blue
            ColorStop {
                value: 4.0,
                color: [0, 180, 80, 255],
            }, // green
            ColorStop {
                value: 8.0,
                color: [200, 220, 0, 255],
            }, // yellow
            ColorStop {
                value: 15.0,
                color: [255, 140, 0, 255],
            }, // orange
            ColorStop {
                value: 30.0,
                color: [220, 0, 0, 255],
            }, // red (heavy)
        ],
        BuiltinColormap::WindSpeed => vec![
            ColorStop {
                value: 0.0,
                color: [0, 160, 0, 255],
            }, // calm green
            ColorStop {
                value: 5.0,
                color: [100, 200, 0, 255],
            }, // yellow-green
            ColorStop {
                value: 10.0,
                color: [200, 200, 0, 255],
            }, // yellow
            ColorStop {
                value: 15.0,
                color: [255, 180, 0, 255],
            }, // orange-yellow
            ColorStop {
                value: 20.0,
                color: [255, 100, 0, 255],
            }, // orange
            ColorStop {
                value: 25.0,
                color: [255, 0, 0, 255],
            }, // red
            ColorStop {
                value: 30.0,
                color: [200, 0, 80, 255],
            }, // crimson
            ColorStop {
                value: 40.0,
                color: [150, 0, 150, 255],
            }, // purple
            ColorStop {
                value: 50.0,
                color: [100, 0, 200, 255],
            }, // violet
        ],
    }
}

/// Resolve a built-in colormap name to its enum variant.
pub fn resolve_builtin(name: &str) -> Option<BuiltinColormap> {
    match name {
        "radar_dbz" => Some(BuiltinColormap::RadarDbz),
        "radar_smhi" => Some(BuiltinColormap::RadarSmhi),
        "radar_fmi" => Some(BuiltinColormap::RadarFmi),
        "radar_bookbinder" => Some(BuiltinColormap::RadarBookbinder),
        "grayscale" => Some(BuiltinColormap::Grayscale),
        "viridis" => Some(BuiltinColormap::Viridis),
        "temperature" => Some(BuiltinColormap::Temperature),
        "precipitation" => Some(BuiltinColormap::Precipitation),
        "precipitation_rate" => Some(BuiltinColormap::PrecipitationRate),
        "wind_speed" => Some(BuiltinColormap::WindSpeed),
        _ => None,
    }
}

/// List all available built-in colormap names.
pub fn builtin_names() -> &'static [&'static str] {
    &[
        "radar_dbz",
        "radar_smhi",
        "radar_fmi",
        "radar_bookbinder",
        "grayscale",
        "viridis",
        "temperature",
        "precipitation",
        "precipitation_rate",
        "wind_speed",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lut_colormap_boundaries() {
        let cmap = LutColorMap::from_builtin(BuiltinColormap::Grayscale, 0.0, 100.0);
        let black = cmap.color(Some(0.0));
        let white = cmap.color(Some(100.0));
        assert_eq!(black, [0, 0, 0, 255]);
        assert_eq!(white, [255, 255, 255, 255]);
    }

    #[test]
    fn test_lut_colormap_nodata() {
        let cmap = LutColorMap::from_builtin(BuiltinColormap::Grayscale, 0.0, 100.0);
        assert_eq!(cmap.color(None), [0, 0, 0, 0]);
    }

    #[test]
    fn test_overlay_colormap_sentinel_and_passthrough() {
        let inner = std::sync::Arc::new(LutColorMap::from_builtin(
            BuiltinColormap::Grayscale,
            0.0,
            100.0,
        ));
        let overlay = OverlayColorMap::new(inner, -9999.0, [60, 60, 60, 255]);
        // The sentinel renders as the fixed overlay colour…
        assert_eq!(overlay.color(Some(-9999.0)), [60, 60, 60, 255]);
        // …every other value delegates to the inner colormap…
        assert_eq!(overlay.color(Some(0.0)), [0, 0, 0, 255]);
        assert_eq!(overlay.color(Some(100.0)), [255, 255, 255, 255]);
        // …and nodata / NaN follow the inner colormap's nodata colour, never
        // the overlay colour (sentinel match is finite-exact).
        assert_eq!(overlay.color(None), [0, 0, 0, 0]);
        assert_eq!(overlay.color(Some(f64::NAN)), [0, 0, 0, 0]);
        // A near-but-not-equal value is NOT the sentinel: it delegates to the
        // inner colormap (here clamped to the min/black end), never the
        // overlay colour.
        let inner = LutColorMap::from_builtin(BuiltinColormap::Grayscale, 0.0, 100.0);
        assert_eq!(overlay.color(Some(-9998.0)), inner.color(Some(-9998.0)));
        assert_ne!(overlay.color(Some(-9998.0)), [60, 60, 60, 255]);
    }

    #[test]
    fn test_lut_colormap_clamp() {
        let cmap = LutColorMap::from_builtin(BuiltinColormap::Grayscale, 0.0, 100.0);
        // Below min should clamp to first color
        assert_eq!(cmap.color(Some(-10.0)), [0, 0, 0, 255]);
        // Above max should clamp to last color
        assert_eq!(cmap.color(Some(200.0)), [255, 255, 255, 255]);
    }

    #[test]
    fn test_parse_hex_color() {
        assert_eq!(parse_hex_color("#FF0000"), Ok([255, 0, 0, 255]));
        assert_eq!(parse_hex_color("#00FF0080"), Ok([0, 255, 0, 128]));
        assert_eq!(parse_hex_color("AABBCC"), Ok([170, 187, 204, 255]));
        assert!(parse_hex_color("#FFF").is_err());
    }

    #[test]
    fn test_linear_colormap() {
        let cmap = LinearColorMap::new(vec![
            ColorStop {
                value: 0.0,
                color: [0, 0, 0, 255],
            },
            ColorStop {
                value: 100.0,
                color: [255, 255, 255, 255],
            },
        ]);
        let mid = cmap.color(Some(50.0));
        assert_eq!(mid, [128, 128, 128, 255]);
    }

    // --- IntegerLutColorMap tests ---

    #[test]
    fn test_integer_lut_matches_linear_colormap() {
        let linear = LinearColorMap::new(vec![
            ColorStop {
                value: 0.0,
                color: [0, 0, 0, 255],
            },
            ColorStop {
                value: 255.0,
                color: [255, 255, 255, 255],
            },
        ]);
        let lut = linear.to_integer_lut(0, 255).unwrap();

        // Every integer value should produce the same color
        for i in 0..=255 {
            assert_eq!(
                lut.color(Some(i as f64)),
                linear.color(Some(i as f64)),
                "mismatch at value {i}"
            );
        }
    }

    #[test]
    fn test_integer_lut_signed_offset() {
        let linear = LinearColorMap::new(vec![
            ColorStop {
                value: -128.0,
                color: [0, 0, 0, 255],
            },
            ColorStop {
                value: 127.0,
                color: [255, 255, 255, 255],
            },
        ]);
        let lut = IntegerLutColorMap::from_colormap(&linear, -128, 127).unwrap();

        // Check boundaries
        assert_eq!(lut.color(Some(-128.0)), linear.color(Some(-128.0)));
        assert_eq!(lut.color(Some(0.0)), linear.color(Some(0.0)));
        assert_eq!(lut.color(Some(127.0)), linear.color(Some(127.0)));

        // Check all values match
        for i in -128..=127_i64 {
            assert_eq!(
                lut.color(Some(i as f64)),
                linear.color(Some(i as f64)),
                "mismatch at value {i}"
            );
        }
    }

    #[test]
    fn test_integer_lut_out_of_range_saturates() {
        let linear = LinearColorMap::new(vec![
            ColorStop {
                value: 0.0,
                color: [100, 100, 100, 255],
            },
            ColorStop {
                value: 10.0,
                color: [200, 200, 200, 255],
            },
        ]);
        let lut = IntegerLutColorMap::from_colormap(&linear, 0, 10).unwrap();

        // Out of range below → first entry (saturate, matching LutColorMap clamp).
        assert_eq!(lut.color(Some(-1.0)), linear.color(Some(0.0)));
        assert_eq!(lut.color(Some(-100.0)), linear.color(Some(0.0)));
        // Out of range above → last entry.
        assert_eq!(lut.color(Some(11.0)), linear.color(Some(10.0)));
        assert_eq!(lut.color(Some(1e9)), linear.color(Some(10.0)));
    }

    #[test]
    fn test_all_colormaps_treat_nan_and_infinity_as_nodata() {
        // All three colormap types must agree on non-finite inputs — otherwise
        // `maybe_wrap_integer_lut` (the #207 wrap) silently changes behaviour
        // when an engine lets NaN reach the colorizer.
        let stops = vec![
            ColorStop {
                value: 0.0,
                color: [255, 0, 0, 255],
            },
            ColorStop {
                value: 10.0,
                color: [0, 255, 0, 255],
            },
        ];
        let lut = LutColorMap::from_stops(&stops, 0.0, 10.0);
        let lin = LinearColorMap::new(stops);
        let int_lut = IntegerLutColorMap::from_colormap(&lin, 0, 10).unwrap();
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(lut.color(Some(v)), [0, 0, 0, 0], "LutColorMap on {v}");
            assert_eq!(lin.color(Some(v)), [0, 0, 0, 0], "LinearColorMap on {v}");
            assert_eq!(
                int_lut.color(Some(v)),
                [0, 0, 0, 0],
                "IntegerLutColorMap on {v}"
            );
        }
    }

    #[test]
    fn test_integer_lut_rounds_nearest_not_toward_zero() {
        // Pins round-nearest in IntegerLutColorMap (not truncate-toward-zero,
        // which would diverge for negative values). We pin behaviour INTERNAL
        // to the LUT here — IntegerLutColorMap has 1 entry per integer unit
        // while LutColorMap uses a fixed 4096-entry table, so the two paths
        // can't be expected to match at non-integer inputs.
        let linear = LinearColorMap::new(vec![
            ColorStop {
                value: -10.0,
                color: [0, 0, 0, 255],
            },
            ColorStop {
                value: 10.0,
                color: [255, 255, 255, 255],
            },
        ]);
        let lut = IntegerLutColorMap::from_colormap(&linear, -10, 10).unwrap();
        // 4.7 rounds up to 5, NOT toward zero (which would yield 4).
        assert_eq!(lut.color(Some(4.7)), lut.color(Some(5.0)));
        assert_ne!(lut.color(Some(4.7)), lut.color(Some(4.0)));
        // -0.7 rounds to the nearer integer (-1), NOT toward zero (which would
        // collapse it onto entry 0 — the bug the rounding fix prevents).
        assert_ne!(lut.color(Some(-0.7)), lut.color(Some(0.0)));
        assert_eq!(lut.color(Some(-0.7)), lut.color(Some(-1.0)));
        // -0.5 is half-way; Rust's `.round()` rounds away from zero in the
        // (non-negative) index-space coordinate after the offset shift, so in
        // value space it rounds toward `max` here.
        assert_eq!(lut.color(Some(-0.5)), lut.color(Some(0.0)));
    }

    #[test]
    fn test_integer_lut_nodata() {
        let linear = LinearColorMap::new(vec![
            ColorStop {
                value: 0.0,
                color: [255, 0, 0, 255],
            },
            ColorStop {
                value: 10.0,
                color: [0, 255, 0, 255],
            },
        ]);
        let lut = IntegerLutColorMap::from_colormap(&linear, 0, 10).unwrap();
        assert_eq!(lut.color(None), [0, 0, 0, 0]);
    }

    /// `from_colormap` must copy the source's `nodata_color`, not hardcode
    /// transparent — otherwise a colormap with grey/opaque nodata would
    /// silently flip to transparent when wrapped.
    #[test]
    fn test_integer_lut_preserves_source_nodata_color() {
        // Mock colormap with a non-default (opaque grey) nodata colour.
        struct GreyNodata;
        impl ColorMap for GreyNodata {
            fn color(&self, value: Option<f64>) -> [u8; 4] {
                match value {
                    None => self.nodata_color(),
                    Some(v) if !v.is_finite() => self.nodata_color(),
                    Some(_) => [255, 0, 0, 255],
                }
            }
            fn nodata_color(&self) -> [u8; 4] {
                [128, 128, 128, 255]
            }
        }

        let lut = IntegerLutColorMap::from_colormap(&GreyNodata, 0, 10).unwrap();
        assert_eq!(lut.color(None), [128, 128, 128, 255]);
        assert_eq!(lut.color(Some(f64::NAN)), [128, 128, 128, 255]);
        assert_eq!(lut.nodata_color(), [128, 128, 128, 255]);

        // Empty-range path (max < min) must propagate it too.
        let empty = IntegerLutColorMap::from_colormap(&GreyNodata, 10, 5).unwrap();
        assert_eq!(empty.color(None), [128, 128, 128, 255]);
        assert_eq!(empty.color(Some(0.0)), [128, 128, 128, 255]);
    }

    #[test]
    fn test_integer_lut_single_value_range() {
        let linear = LinearColorMap::new(vec![
            ColorStop {
                value: 0.0,
                color: [0, 0, 0, 255],
            },
            ColorStop {
                value: 100.0,
                color: [255, 255, 255, 255],
            },
        ]);
        let lut = IntegerLutColorMap::from_colormap(&linear, 50, 50).unwrap();
        assert_eq!(lut.color(Some(50.0)), linear.color(Some(50.0)));
        // Out of range → saturate to the single available entry.
        assert_eq!(lut.color(Some(49.0)), linear.color(Some(50.0)));
        assert_eq!(lut.color(Some(51.0)), linear.color(Some(50.0)));
    }

    #[test]
    fn test_integer_lut_full_uint16_range() {
        let linear = LinearColorMap::new(vec![
            ColorStop {
                value: 0.0,
                color: [0, 0, 0, 255],
            },
            ColorStop {
                value: 65535.0,
                color: [255, 255, 255, 255],
            },
        ]);
        let lut = IntegerLutColorMap::from_colormap(&linear, 0, 65535).unwrap();

        // Check boundaries
        assert_eq!(lut.color(Some(0.0)), [0, 0, 0, 255]);
        assert_eq!(lut.color(Some(65535.0)), [255, 255, 255, 255]);
        // Check midpoint
        assert_eq!(lut.color(Some(32768.0)), linear.color(Some(32768.0)));
    }

    #[test]
    fn test_integer_lut_full_int16_range() {
        let linear = LinearColorMap::new(vec![
            ColorStop {
                value: -32768.0,
                color: [0, 0, 0, 255],
            },
            ColorStop {
                value: 32767.0,
                color: [255, 255, 255, 255],
            },
        ]);
        let lut = IntegerLutColorMap::from_colormap(&linear, -32768, 32767).unwrap();

        assert_eq!(lut.color(Some(-32768.0)), [0, 0, 0, 255]);
        assert_eq!(lut.color(Some(32767.0)), [255, 255, 255, 255]);
        assert_eq!(lut.color(Some(0.0)), linear.color(Some(0.0)));
    }

    #[test]
    fn test_integer_lut_rejects_too_large_range() {
        let linear = LinearColorMap::new(vec![
            ColorStop {
                value: 0.0,
                color: [0, 0, 0, 255],
            },
            ColorStop {
                value: 1.0,
                color: [255, 255, 255, 255],
            },
        ]);
        // 65537 entries — one too many
        assert!(IntegerLutColorMap::from_colormap(&linear, 0, 65536).is_none());
    }

    #[test]
    fn test_integer_lut_empty_range() {
        let linear = LinearColorMap::new(vec![ColorStop {
            value: 0.0,
            color: [255, 0, 0, 255],
        }]);
        // min > max → empty
        let lut = IntegerLutColorMap::from_colormap(&linear, 10, 5).unwrap();
        assert_eq!(lut.color(Some(7.0)), [0, 0, 0, 0]);
    }

    #[test]
    fn test_integer_lut_from_builtin_lut() {
        let builtin = LutColorMap::from_builtin(BuiltinColormap::RadarDbz, 0.0, 70.0);
        let int_lut = builtin.to_integer_lut(0, 70).unwrap();

        // Verify key values match
        for v in [0, 5, 10, 25, 35, 50, 70] {
            assert_eq!(
                int_lut.color(Some(v as f64)),
                builtin.color(Some(v as f64)),
                "mismatch at dBZ {v}"
            );
        }
    }
}
