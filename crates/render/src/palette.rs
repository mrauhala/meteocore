//! Named palette model — the single source of truth for built-in colormaps
//! and the registry that user-defined palettes load into.
//!
//! A [`Palette`] is a named list of color stops plus an interpolation mode.
//! Built-ins are rows in one static table ([`builtin_palettes`]); user
//! palettes (from `[[colormaps]]` config or a colormaps directory) are
//! inserted into a [`PaletteRegistry`] at config load. Everything that
//! resolves a colormap by name goes through that registry (or
//! [`builtin_palette`] for builtins only), so there is exactly one list to
//! maintain.
//!
//! The legacy `BuiltinColormap` enum in [`crate::colormap`] is a shim over
//! this table, kept for existing `LutColorMap::from_builtin` callers.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use crate::colormap::{sample_stops, ColorStop};

/// How colors are produced between stops.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Interpolation {
    /// Linear RGBA interpolation between bracketing stops (the historical
    /// behavior).
    #[default]
    Linear,
    /// Discrete classes: a value takes the color of the highest stop at or
    /// below it. Values below the first stop clamp to the first stop's
    /// color, matching the linear mode's clamp semantics.
    Step,
}

/// A named, reusable color palette.
///
/// Stops are kept sorted ascending by value (enforced by [`Palette::new`]).
#[derive(Clone, Debug)]
pub struct Palette {
    /// Registry key, referenced by `colormap = "<name>"` in config.
    pub name: String,
    /// Human-readable title (legend/UI).
    pub title: Option<String>,
    /// Sorted ascending by value.
    pub stops: Vec<ColorStop>,
    pub interpolation: Interpolation,
    /// True when the stops span a normalized 0..1 domain (viridis,
    /// grayscale) rather than physical values. Such palettes need an
    /// explicit or derived value range to render meaningfully —
    /// [`Palette::default_range`] returns `None` for them.
    pub normalized: bool,
    /// Explicit nodata color (e.g. from a `.cpt` `N` line). `None` = the
    /// colormap default (fully transparent).
    pub nodata_color: Option<[u8; 4]>,
}

impl Palette {
    /// Create a palette, sorting the stops ascending by value.
    pub fn new(
        name: impl Into<String>,
        stops: Vec<ColorStop>,
        interpolation: Interpolation,
    ) -> Self {
        let mut stops = stops;
        stops.sort_by(|a, b| a.value.total_cmp(&b.value));
        Self {
            name: name.into(),
            title: None,
            stops,
            interpolation,
            normalized: false,
            nodata_color: None,
        }
    }

    /// The value range implied by the stops (first..last), or `None` when
    /// the palette is [`normalized`](Self::normalized) or has fewer than two
    /// distinct stop values — callers must then supply a range themselves.
    pub fn default_range(&self) -> Option<(f64, f64)> {
        if self.normalized {
            return None;
        }
        match (self.stops.first(), self.stops.last()) {
            (Some(first), Some(last)) if last.value > first.value => {
                Some((first.value, last.value))
            }
            _ => None,
        }
    }

    /// Sample the palette color at a physical value (no range scaling).
    pub fn sample(&self, value: f64) -> [u8; 4] {
        sample_stops(&self.stops, value, self.interpolation)
    }
}

// ---------------------------------------------------------------------------
// Built-in palette table
// ---------------------------------------------------------------------------

/// One row of the static builtin table. Stop data lives here as plain
/// tuples so the whole palette is a single `static` — [`builtin_palettes`]
/// materializes `Palette` values from it once.
struct BuiltinDef {
    name: &'static str,
    title: &'static str,
    normalized: bool,
    stops: &'static [(f64, [u8; 4])],
}

/// THE list of built-in palettes. Adding a palette = adding one row here;
/// name lookup, the names list, and (for the legacy 12) the
/// `BuiltinColormap` shim all derive from this table.
static BUILTIN_DEFS: &[BuiltinDef] = &[
    BuiltinDef {
        name: "radar_dbz",
        title: "Radar reflectivity (dBZ)",
        normalized: false,
        stops: &[
            (0.0, [0, 0, 0, 0]),          // transparent (no echo)
            (5.0, [0, 0, 0, 0]),          // transparent (below threshold)
            (5.1, [0, 128, 255, 255]),    // light blue
            (15.0, [0, 200, 255, 255]),   // cyan
            (25.0, [0, 200, 0, 255]),     // green
            (30.0, [0, 255, 0, 255]),     // bright green
            (35.0, [255, 255, 0, 255]),   // yellow
            (40.0, [255, 200, 0, 255]),   // orange-yellow
            (45.0, [255, 128, 0, 255]),   // orange
            (50.0, [255, 0, 0, 255]),     // red
            (55.0, [200, 0, 0, 255]),     // dark red
            (60.0, [180, 0, 180, 255]),   // magenta
            (70.0, [255, 255, 255, 255]), // white (extreme)
        ],
    },
    BuiltinDef {
        name: "radar_smhi",
        title: "SMHI radar reflectivity",
        normalized: false,
        // Gray tones below 5 dBZ, then blue → green → yellow → orange →
        // red → magenta → cyan, with per-dBZ colors.
        stops: &[
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
        ],
    },
    BuiltinDef {
        name: "radar_fmi",
        title: "FMI radar reflectivity",
        normalized: false,
        // FMI summer radar reflectivity. Converted from SLD raw values
        // to dBZ via: dBZ = raw * 0.5 - 32. Below 5 dBZ is transparent.
        stops: &[
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
        ],
    },
    BuiltinDef {
        name: "radar_bookbinder",
        title: "Bookbinder radar reflectivity",
        normalized: false,
        // Bookbinder 8-bit Z curve (Evan Bookbinder, WFO SGF).
        // Converted from SLD raw values to dBZ via: dBZ = raw * 0.5 - 32.
        stops: &[
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
        ],
    },
    BuiltinDef {
        name: "grayscale",
        title: "Grayscale",
        normalized: true,
        stops: &[(0.0, [0, 0, 0, 255]), (1.0, [255, 255, 255, 255])],
    },
    BuiltinDef {
        name: "viridis",
        title: "Viridis",
        normalized: true,
        stops: &[
            (0.0, [68, 1, 84, 255]),
            (0.125, [72, 36, 117, 255]),
            (0.25, [56, 88, 140, 255]),
            (0.375, [38, 130, 142, 255]),
            (0.5, [31, 158, 137, 255]),
            (0.625, [78, 178, 101, 255]),
            (0.75, [148, 197, 56, 255]),
            (0.875, [220, 215, 30, 255]),
            (1.0, [253, 231, 37, 255]),
        ],
    },
    BuiltinDef {
        name: "temperature",
        title: "Temperature (°C)",
        normalized: false,
        stops: &[
            (-40.0, [40, 0, 120, 255]),  // deep purple (extreme cold)
            (-30.0, [0, 0, 180, 255]),   // dark blue
            (-20.0, [0, 60, 255, 255]),  // blue
            (-10.0, [0, 160, 255, 255]), // light blue
            (0.0, [0, 220, 220, 255]),   // cyan
            (10.0, [0, 200, 0, 255]),    // green
            (20.0, [200, 200, 0, 255]),  // yellow
            (30.0, [255, 128, 0, 255]),  // orange
            (40.0, [255, 0, 0, 255]),    // red
            (50.0, [180, 0, 0, 255]),    // dark red
        ],
    },
    BuiltinDef {
        name: "precipitation",
        title: "Precipitation accumulation (mm)",
        normalized: false,
        stops: &[
            (0.0, [0, 0, 0, 0]),          // transparent (no precip)
            (0.1, [170, 220, 255, 255]),  // very light blue
            (0.5, [100, 180, 255, 255]),  // light blue
            (1.0, [50, 130, 255, 255]),   // blue
            (2.0, [0, 80, 255, 255]),     // medium blue
            (5.0, [0, 40, 200, 255]),     // dark blue
            (10.0, [120, 0, 200, 255]),   // purple
            (20.0, [200, 0, 150, 255]),   // magenta
            (50.0, [255, 255, 255, 255]), // white (extreme)
        ],
    },
    BuiltinDef {
        name: "precipitation_rate",
        title: "Precipitation rate (mm/h)",
        normalized: false,
        stops: &[
            (0.0, [0, 0, 0, 0]),         // transparent (no rain)
            (0.1, [200, 240, 255, 200]), // very light cyan (drizzle)
            (0.5, [100, 210, 255, 255]), // light cyan
            (1.0, [30, 170, 255, 255]),  // cyan-blue
            (2.0, [0, 120, 200, 255]),   // blue
            (4.0, [0, 180, 80, 255]),    // green
            (8.0, [200, 220, 0, 255]),   // yellow
            (15.0, [255, 140, 0, 255]),  // orange
            (30.0, [220, 0, 0, 255]),    // red (heavy)
        ],
    },
    BuiltinDef {
        name: "wind_speed",
        title: "Wind speed (m/s)",
        normalized: false,
        stops: &[
            (0.0, [0, 160, 0, 255]),    // calm green
            (5.0, [100, 200, 0, 255]),  // yellow-green
            (10.0, [200, 200, 0, 255]), // yellow
            (15.0, [255, 180, 0, 255]), // orange-yellow
            (20.0, [255, 100, 0, 255]), // orange
            (25.0, [255, 0, 0, 255]),   // red
            (30.0, [200, 0, 80, 255]),  // crimson
            (40.0, [150, 0, 150, 255]), // purple
            (50.0, [100, 0, 200, 255]), // violet
        ],
    },
    BuiltinDef {
        name: "cap_severity",
        title: "CAP alert severity",
        normalized: false,
        // Integer severity codes 0–4 with a semi-transparent alpha so the
        // alert fill overlays a basemap (Unknown=0 … Extreme=4). Used by
        // the engine-cap alert map layers (#396).
        stops: &[
            (0.0, [120, 120, 120, 200]), // Unknown — grey
            (1.0, [38, 166, 91, 200]),   // Minor — green
            (2.0, [241, 196, 15, 200]),  // Moderate — yellow
            (3.0, [230, 126, 34, 200]),  // Severe — orange
            (4.0, [192, 57, 43, 200]),   // Extreme — red
        ],
    },
    BuiltinDef {
        name: "lightning_age",
        title: "Lightning strike age (min)",
        normalized: false,
        // Value = strike age in MINUTES (#504): fresh strikes near-white,
        // aging through orange and red to dark violet at the window edge.
        stops: &[
            (0.0, [255, 255, 240, 255]), // just struck — near-white
            (5.0, [255, 236, 80, 255]),  // ≤5 min — bright yellow
            (15.0, [255, 160, 40, 255]), // orange
            (30.0, [225, 60, 50, 255]),  // red
            (45.0, [160, 40, 120, 255]), // magenta
            (60.0, [90, 30, 130, 255]),  // window edge — dark violet
        ],
    },
    BuiltinDef {
        name: "radial_velocity",
        title: "Radial velocity (m/s)",
        normalized: false,
        // Doppler radial velocity — diverging blue → white → red about
        // zero (toward/away). Stops match the ±48 m/s dealiased Nyquist
        // band used by the Nordic PVOL volumes.
        stops: &[
            (-48.0, [26, 26, 255, 255]),   // strong toward — deep blue
            (-24.0, [102, 204, 255, 255]), // toward — light blue
            (0.0, [245, 245, 245, 255]),   // stationary — near-white
            (24.0, [255, 140, 102, 255]),  // away — light red
            (48.0, [204, 0, 0, 255]),      // strong away — dark red
        ],
    },
    BuiltinDef {
        name: "pressure",
        title: "Mean sea-level pressure (hPa)",
        normalized: false,
        stops: &[
            (950.0, [85, 0, 130, 255]),     // deep low — purple
            (970.0, [40, 60, 200, 255]),    // blue
            (990.0, [0, 150, 255, 255]),    // light blue
            (1000.0, [100, 200, 150, 255]), // green
            (1013.0, [220, 220, 210, 255]), // standard atmosphere — neutral
            (1025.0, [240, 180, 80, 255]),  // orange
            (1040.0, [220, 90, 40, 255]),   // strong high — red-orange
            (1050.0, [170, 30, 30, 255]),   // extreme high — dark red
        ],
    },
    BuiltinDef {
        name: "humidity",
        title: "Relative humidity (%)",
        normalized: false,
        stops: &[
            (0.0, [150, 100, 50, 255]),   // bone dry — brown
            (20.0, [200, 160, 110, 255]), // tan
            (40.0, [230, 220, 180, 255]), // pale
            (60.0, [170, 210, 160, 255]), // light green
            (80.0, [90, 180, 140, 255]),  // green
            (100.0, [30, 120, 180, 255]), // saturated — blue
        ],
    },
    BuiltinDef {
        name: "cloud_cover",
        title: "Cloud cover (%)",
        normalized: false,
        // White with increasing opacity so the layer reads naturally over
        // a basemap: clear sky is transparent, overcast near-opaque gray.
        stops: &[
            (0.0, [255, 255, 255, 0]),     // clear — transparent
            (12.5, [255, 255, 255, 64]),   // few
            (25.0, [250, 250, 250, 110]),  // scattered
            (50.0, [240, 240, 240, 170]),  // broken
            (75.0, [225, 225, 225, 215]),  // mostly overcast
            (100.0, [200, 200, 205, 255]), // overcast — light gray
        ],
    },
];

fn materialize(def: &BuiltinDef) -> Palette {
    Palette {
        name: def.name.to_string(),
        title: Some(def.title.to_string()),
        stops: def
            .stops
            .iter()
            .map(|&(value, color)| ColorStop { value, color })
            .collect(),
        interpolation: Interpolation::Linear,
        normalized: def.normalized,
        nodata_color: None,
    }
}

/// All built-in palettes, materialized once from the static table.
pub fn builtin_palettes() -> &'static [Palette] {
    static PALETTES: LazyLock<Vec<Palette>> =
        LazyLock::new(|| BUILTIN_DEFS.iter().map(materialize).collect());
    PALETTES.as_slice()
}

/// Look up a built-in palette by name.
pub fn builtin_palette(name: &str) -> Option<&'static Palette> {
    builtin_palettes().iter().find(|p| p.name == name)
}

/// All built-in palette names, in table order.
pub fn builtin_names() -> &'static [&'static str] {
    static NAMES: LazyLock<Vec<&'static str>> =
        LazyLock::new(|| BUILTIN_DEFS.iter().map(|d| d.name).collect());
    NAMES.as_slice()
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Outcome of a successful [`PaletteRegistry::insert`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteInsert {
    /// The palette was added under a new name.
    Added,
    /// The palette replaced a built-in of the same name. Allowed (it lets a
    /// deployment restyle e.g. `temperature` house-wide), but the caller
    /// should surface a warning.
    ShadowedBuiltin,
}

/// Name → palette lookup: built-ins plus user-defined entries.
///
/// Collision rules: a user palette may shadow a built-in name (the caller
/// gets [`PaletteInsert::ShadowedBuiltin`] back and should WARN); two user
/// palettes with the same name are an error.
#[derive(Clone, Debug, Default)]
pub struct PaletteRegistry {
    map: HashMap<String, Arc<Palette>>,
    user_defined: HashSet<String>,
}

impl PaletteRegistry {
    /// A registry pre-populated with every built-in palette.
    pub fn with_builtins() -> Self {
        let map = builtin_palettes()
            .iter()
            .map(|p| (p.name.clone(), Arc::new(p.clone())))
            .collect();
        Self {
            map,
            user_defined: HashSet::new(),
        }
    }

    /// Insert a user-defined palette. Errors on an empty name or a name
    /// already used by another user-defined palette.
    pub fn insert(&mut self, palette: Palette) -> Result<PaletteInsert, String> {
        let name = palette.name.clone();
        if name.is_empty() {
            return Err("colormap name must not be empty".to_string());
        }
        if self.user_defined.contains(&name) {
            return Err(format!("duplicate colormap name '{name}'"));
        }
        let shadowed = self.map.contains_key(&name);
        self.map.insert(name.clone(), Arc::new(palette));
        self.user_defined.insert(name);
        Ok(if shadowed {
            PaletteInsert::ShadowedBuiltin
        } else {
            PaletteInsert::Added
        })
    }

    /// Look up a palette by name.
    pub fn get(&self, name: &str) -> Option<Arc<Palette>> {
        self.map.get(name).cloned()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.map.contains_key(name)
    }

    /// All registered names, sorted.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.map.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the table against accidental edits: every palette keeps its
    /// stop count and first/last stop values.
    #[test]
    fn builtin_table_snapshot() {
        let expected: &[(&str, usize, f64, f64)] = &[
            ("radar_dbz", 13, 0.0, 70.0),
            ("radar_smhi", 32, -30.0, 70.0),
            ("radar_fmi", 12, -32.0, 58.0),
            ("radar_bookbinder", 21, -32.0, 94.5),
            ("grayscale", 2, 0.0, 1.0),
            ("viridis", 9, 0.0, 1.0),
            ("temperature", 10, -40.0, 50.0),
            ("precipitation", 9, 0.0, 50.0),
            ("precipitation_rate", 9, 0.0, 30.0),
            ("wind_speed", 9, 0.0, 50.0),
            ("cap_severity", 5, 0.0, 4.0),
            ("lightning_age", 6, 0.0, 60.0),
            ("radial_velocity", 5, -48.0, 48.0),
            ("pressure", 8, 950.0, 1050.0),
            ("humidity", 6, 0.0, 100.0),
            ("cloud_cover", 6, 0.0, 100.0),
        ];
        assert_eq!(builtin_palettes().len(), expected.len());
        assert_eq!(builtin_names().len(), expected.len());
        for (name, count, first, last) in expected {
            let p = builtin_palette(name).unwrap_or_else(|| panic!("missing builtin '{name}'"));
            assert_eq!(p.stops.len(), *count, "{name} stop count");
            assert_eq!(p.stops.first().unwrap().value, *first, "{name} first stop");
            assert_eq!(p.stops.last().unwrap().value, *last, "{name} last stop");
            assert!(builtin_names().contains(name));
            // Table stops must be pre-sorted — sample()/default_range()
            // assume ascending order.
            assert!(
                p.stops.windows(2).all(|w| w[0].value <= w[1].value),
                "{name} stops not ascending"
            );
        }
    }

    /// Spot-pin exact colors of the most load-bearing palette so a data
    /// typo in the table move is caught, not just structural drift.
    #[test]
    fn radar_dbz_exact_stops() {
        let p = builtin_palette("radar_dbz").unwrap();
        assert_eq!(p.stops[0].color, [0, 0, 0, 0]);
        assert_eq!(p.stops[2].value, 5.1);
        assert_eq!(p.stops[2].color, [0, 128, 255, 255]);
        assert_eq!(p.stops[6].value, 35.0);
        assert_eq!(p.stops[6].color, [255, 255, 0, 255]);
        assert_eq!(p.stops[12].color, [255, 255, 255, 255]);
    }

    #[test]
    fn normalized_flags_and_default_range() {
        assert!(builtin_palette("viridis").unwrap().normalized);
        assert!(builtin_palette("grayscale").unwrap().normalized);
        assert_eq!(builtin_palette("viridis").unwrap().default_range(), None);
        assert_eq!(
            builtin_palette("radar_dbz").unwrap().default_range(),
            Some((0.0, 70.0))
        );
        assert_eq!(
            builtin_palette("radial_velocity").unwrap().default_range(),
            Some((-48.0, 48.0))
        );
        for p in builtin_palettes() {
            assert!(!p.normalized || p.default_range().is_none());
        }
    }

    #[test]
    fn palette_new_sorts_stops() {
        let p = Palette::new(
            "reversed",
            vec![
                ColorStop {
                    value: 10.0,
                    color: [255, 0, 0, 255],
                },
                ColorStop {
                    value: 0.0,
                    color: [0, 0, 255, 255],
                },
            ],
            Interpolation::Linear,
        );
        assert_eq!(p.stops[0].value, 0.0);
        assert_eq!(p.stops[1].value, 10.0);
        assert_eq!(p.default_range(), Some((0.0, 10.0)));
    }

    #[test]
    fn palette_sample_linear_and_step() {
        let stops = vec![
            ColorStop {
                value: 0.0,
                color: [0, 0, 0, 255],
            },
            ColorStop {
                value: 10.0,
                color: [100, 100, 100, 255],
            },
            ColorStop {
                value: 20.0,
                color: [200, 200, 200, 255],
            },
        ];
        let linear = Palette::new("lin", stops.clone(), Interpolation::Linear);
        assert_eq!(linear.sample(5.0), [50, 50, 50, 255]);

        let step = Palette::new("step", stops, Interpolation::Step);
        // Between stops: color of the highest stop at or below the value.
        assert_eq!(step.sample(5.0), [0, 0, 0, 255]);
        assert_eq!(step.sample(9.999), [0, 0, 0, 255]);
        assert_eq!(step.sample(10.0), [100, 100, 100, 255]);
        assert_eq!(step.sample(19.0), [100, 100, 100, 255]);
        assert_eq!(step.sample(25.0), [200, 200, 200, 255]);
        // Below the first stop clamps to the first stop's color.
        assert_eq!(step.sample(-5.0), [0, 0, 0, 255]);
    }

    #[test]
    fn registry_builtins_and_user_inserts() {
        let mut reg = PaletteRegistry::with_builtins();
        assert_eq!(reg.names().len(), builtin_palettes().len());
        assert!(reg.contains("radar_dbz"));
        assert!(reg.get("nope").is_none());

        // New user name → Added.
        let custom = Palette::new(
            "house_style",
            vec![
                ColorStop {
                    value: 0.0,
                    color: [1, 2, 3, 255],
                },
                ColorStop {
                    value: 1.0,
                    color: [4, 5, 6, 255],
                },
            ],
            Interpolation::Linear,
        );
        assert_eq!(reg.insert(custom.clone()), Ok(PaletteInsert::Added));

        // Same user name again → error.
        assert!(reg.insert(custom).is_err());

        // Shadowing a builtin → allowed, flagged.
        let shadow = Palette::new(
            "temperature",
            vec![
                ColorStop {
                    value: 0.0,
                    color: [9, 9, 9, 255],
                },
                ColorStop {
                    value: 1.0,
                    color: [8, 8, 8, 255],
                },
            ],
            Interpolation::Linear,
        );
        assert_eq!(reg.insert(shadow), Ok(PaletteInsert::ShadowedBuiltin));
        // The user palette wins on lookup.
        assert_eq!(reg.get("temperature").unwrap().stops.len(), 2);
        // Shadowing the same builtin twice is a duplicate user name.
        let shadow2 = Palette::new("temperature", vec![], Interpolation::Linear);
        assert!(reg.insert(shadow2).is_err());

        // Empty name rejected.
        assert!(reg
            .insert(Palette::new("", vec![], Interpolation::Linear))
            .is_err());
    }
}
