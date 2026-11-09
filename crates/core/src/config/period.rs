//! The period-product derived collection's config (`engine_type =
//! "period"`, #1379): rolling-window statistics over another collection's
//! frames, radar rain accumulation first. The engine (`engine-period`)
//! checks the values when it is built; a bad value fails the collection.

use serde::Deserialize;

/// `[collections.period]`: one statistic over rolling windows of a source
/// collection's frames, one parameter per window plus its coverage.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PeriodConfig {
    /// Collection id of the gridded observation collection whose frames are
    /// summarised (radar, satellite, a station grid), in the same config.
    pub source: String,
    /// The source parameter. Default: the source's default parameter.
    #[serde(default)]
    pub parameter: Option<String>,
    /// `sum`, `mean` or `count_above`.
    #[serde(default = "default_statistic")]
    pub statistic: String,
    /// Window lengths, ISO 8601 durations (`PT1H`, `PT24H`). Each window
    /// ends at the source's latest timestep and is a parameter of its own.
    pub windows: Vec<String>,
    /// Convert reflectivity (dBZ) to rain rate (mm/h) with `Z = a·R^b`
    /// before the statistic. Absent: the source values are used as they are.
    #[serde(default)]
    pub rain_rate: Option<RainRateConfig>,
    /// `count_above`'s threshold, in the statistic's input unit (mm/h with
    /// `rain_rate`, else the source's). Required for `count_above` only.
    #[serde(default)]
    pub threshold: Option<f64>,
    /// The share of a window its frames must cover for it to be served.
    #[serde(default = "default_min_coverage")]
    pub min_coverage: f64,
    /// The source's frame spacing, ISO 8601: a frame counts for its spacing
    /// to the next one, at most this long. Default: the median spacing of
    /// the source's newest frames.
    #[serde(default)]
    pub cadence: Option<String>,
    /// How often the source is checked for new frames (seconds).
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Cells of the working grid at most. The grid is the source's own
    /// cells while they fit, halved along the longer axis until they do
    /// otherwise. The default keeps a 36 M-cell composite native; every
    /// window costs 6 bytes a cell.
    #[serde(default = "default_max_pixels")]
    pub max_pixels: usize,
    /// Source frames one poll cycle reads at most.
    #[serde(default = "default_max_reads_per_cycle")]
    pub max_reads_per_cycle: usize,
    /// Cells one source read asks for at most: a frame is read in strips of
    /// whole rows of the working grid, at least one row each, and folded
    /// into the windows strip by strip. 4 Mi cells is 64 MiB of a source's
    /// boxed `f64` values, the most a poll holds whatever the source type.
    #[serde(default = "default_read_strip_cells")]
    pub read_strip_cells: usize,
}

/// `rain_rate = { a, b, min_dbz }`: the Z–R relation `Z = a·R^b`, with Z in
/// mm⁶ m⁻³ and R in mm/h. Marshall–Palmer by default.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RainRateConfig {
    #[serde(default = "default_zr_a")]
    pub a: f64,
    #[serde(default = "default_zr_b")]
    pub b: f64,
    /// Reflectivity below this counts as no rain (0 mm/h). The default,
    /// 7 dBZ, is 0.1 mm/h under Marshall–Palmer.
    #[serde(default = "default_min_dbz")]
    pub min_dbz: f64,
}

fn default_statistic() -> String {
    "sum".to_string()
}

fn default_min_coverage() -> f64 {
    0.9
}

fn default_poll_interval() -> u64 {
    30
}

fn default_max_pixels() -> usize {
    40_000_000
}

fn default_max_reads_per_cycle() -> usize {
    16
}

fn default_read_strip_cells() -> usize {
    4 * 1024 * 1024
}

fn default_zr_a() -> f64 {
    200.0
}

fn default_zr_b() -> f64 {
    1.6
}

fn default_min_dbz() -> f64 {
    7.0
}
