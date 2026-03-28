use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub server: ServerSettings,
    pub collections: Vec<CollectionConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    /// External base URL for generating absolute links (e.g. "https://api.example.com").
    /// If not set, defaults to "http://{host}:{port}".
    pub base_url: Option<String>,
}

impl ServerSettings {
    /// Resolved base URL with no trailing slash.
    ///
    /// Priority: `BASE_URL` env var > config `base_url` field > `http://{host}:{port}`.
    pub fn base_url(&self) -> String {
        if let Ok(url) = std::env::var("BASE_URL") {
            return url.trim_end_matches('/').to_string();
        }
        match &self.base_url {
            Some(url) => url.trim_end_matches('/').to_string(),
            None => format!("http://{}:{}", self.host, self.port),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CollectionConfig {
    pub id: String,
    pub title: String,
    pub description: String,
    /// Path or URL to the data source. Required for csv/geojson engines.
    /// Optional for geotiff when endpoint+bucket are specified in [geotiff].
    #[serde(default)]
    pub data_path: Option<String>,
    #[serde(default = "default_apis")]
    pub apis: Vec<String>,
    #[serde(default = "default_engine_type")]
    pub engine_type: String,
    /// GeoTIFF-specific configuration. Required when engine_type = "geotiff".
    pub geotiff: Option<GeoTiffConfig>,
    /// WMS map rendering configuration. Required when apis contains "wms".
    pub wms: Option<WmsConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WmsConfig {
    /// Built-in colormap name for the default style (e.g., "radar_dbz", "viridis").
    /// Ignored if color_stops are provided.
    #[serde(default = "default_colormap")]
    pub colormap: String,
    /// Inline color stops for the default style. Overrides the built-in colormap.
    #[serde(default)]
    pub color_stops: Vec<ColorStop>,
    /// Minimum value for the colormap range. Overrides the colormap's built-in range.
    pub min: Option<f64>,
    /// Maximum value for the colormap range. Overrides the colormap's built-in range.
    pub max: Option<f64>,
    /// Named styles in addition to the default. Each style has its own colormap.
    #[serde(default)]
    pub styles: Vec<WmsStyle>,
    /// Rendered image cache size in MB. Default: 128.
    #[serde(default = "default_rendered_cache_mb")]
    pub rendered_cache_mb: u64,
}

/// A named WMS style with its own colormap configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct WmsStyle {
    pub name: String,
    pub title: Option<String>,
    /// Built-in colormap name.
    pub colormap: Option<String>,
    /// Custom color stops (overrides colormap).
    #[serde(default)]
    pub color_stops: Vec<ColorStop>,
    /// Minimum value for the colormap range.
    pub min: Option<f64>,
    /// Maximum value for the colormap range.
    pub max: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ColorStop {
    pub value: f64,
    /// Color in "#RRGGBB" or "#RRGGBBAA" hex format.
    pub color: String,
}

fn default_colormap() -> String {
    "viridis".to_string()
}

fn default_rendered_cache_mb() -> u64 {
    512
}

#[derive(Debug, Clone, Deserialize)]
pub struct GeoTiffConfig {
    /// Simple filename template with strftime placeholders.
    /// E.g. `"OPERA@%Y%m%dT%H%M@0@ACRR.tiff"` or `"radar_%Y%m%dT%H%MZ.tif"`
    /// Auto-derives regex and timestamp format. Preferred over filename_pattern.
    pub filename_template: Option<String>,
    /// Regex pattern with a named capture group `timestamp` for extracting
    /// timestamps from filenames. E.g. `radar_(?P<timestamp>\d{8}T\d{4}Z)\.tif`
    /// Only needed for complex patterns that filename_template can't express.
    pub filename_pattern: Option<String>,
    /// chrono strftime format for parsing the captured timestamp string.
    /// E.g. `%Y%m%dT%H%MZ`. Only needed when using filename_pattern.
    pub timestamp_format: Option<String>,
    /// The parameter name this collection represents. E.g. "reflectivity"
    pub parameter: String,
    /// Unit of measurement. E.g. "dBZ"
    pub unit: String,
    /// Directory poll interval in seconds. Default: 30
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Glob patterns for files to exclude (e.g. temporary files).
    /// Default: ["*.tmp", "*.part"]
    #[serde(default = "default_exclude_patterns")]
    pub exclude_patterns: Vec<String>,
    /// Maximum number of files to keep in the catalog (most recent by timestamp).
    /// Default: None (no limit). Useful for S3 sources to avoid downloading
    /// hundreds of files. E.g. `max_files = 24` keeps the latest 2 hours of
    /// 5-minute radar data.
    pub max_files: Option<usize>,
    /// Tile cache size in megabytes for remote COG byte-range reads.
    /// Caches compressed tile bytes to avoid repeated S3/HTTP fetches.
    /// Default: 64 MB (~3700 tiles). Set to 0 to disable.
    #[serde(default = "default_tile_cache_mb")]
    pub tile_cache_mb: u64,

    /// Band number to read (1-based). Default: 1.
    /// For multi-band files, selects which band contains the parameter values.
    /// E.g. OPERA radar files have band 1 = data, band 2 = quality.
    #[serde(default = "default_band")]
    pub band: u32,

    /// Override nodata value. Takes precedence over the file's GDAL_NODATA tag.
    /// Use when files lack a nodata tag (e.g., SMHI radar uses 255 but doesn't declare it).
    pub nodata: Option<f64>,
    /// Override scale factor. Takes precedence over the file's GDAL_METADATA SCALE.
    /// Physical value = raw * scale + offset.
    pub scale: Option<f64>,
    /// Override offset. Takes precedence over the file's GDAL_METADATA OFFSET.
    pub offset: Option<f64>,

    /// S3-compatible endpoint URL. When set with `bucket`, replaces `data_path`
    /// for remote access. E.g. `"https://s3.waw3-1.cloudferro.com"`
    pub endpoint: Option<String>,
    /// S3 bucket name. Required when `endpoint` is set.
    pub bucket: Option<String>,
    /// Object prefix pattern, optionally with strftime date templates.
    /// E.g. `"%Y/%m/%d/OPERA/COMP/"` expands to `"2026/03/25/OPERA/COMP/"`.
    /// Re-evaluated on each poll cycle so it stays current across date boundaries.
    pub prefix_pattern: Option<String>,
    /// ISO 8601 duration defining the time window for file selection.
    /// Negative = past (observations), positive = future (forecasts).
    /// E.g. `-PT2H` keeps files from the past 2 hours, `PT6H` keeps the next 6 hours.
    /// Also determines how many date-prefixes to scan automatically.
    /// When not set, all files are kept (subject to max_files).
    pub time_window: Option<String>,
    /// Number of days to scan when prefix_pattern contains date templates.
    /// Default: auto-derived from time_window. Override if needed.
    pub scan_days: Option<u32>,
}

fn default_tile_cache_mb() -> u64 {
    256
}

fn default_band() -> u32 {
    1
}

fn default_poll_interval() -> u64 {
    30
}

fn default_exclude_patterns() -> Vec<String> {
    vec!["*.tmp".to_string(), "*.part".to_string()]
}

fn default_engine_type() -> String {
    "csv".to_string()
}

fn default_apis() -> Vec<String> {
    vec!["edr".to_string()]
}

impl ServerConfig {
    pub fn from_file(path: &str) -> Result<Self, crate::error::DataServerError> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            crate::error::DataServerError::Config(format!("Failed to read {path}: {e}"))
        })?;
        toml::from_str(&content).map_err(|e| {
            crate::error::DataServerError::Config(format!("Failed to parse config: {e}"))
        })
    }
}
