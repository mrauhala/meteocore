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
    pub fn base_url(&self) -> String {
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
    pub data_path: String,
    #[serde(default = "default_apis")]
    pub apis: Vec<String>,
    #[serde(default = "default_engine_type")]
    pub engine_type: String,
    /// GeoTIFF-specific configuration. Required when engine_type = "geotiff".
    pub geotiff: Option<GeoTiffConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GeoTiffConfig {
    /// Regex pattern with a named capture group `timestamp` for extracting
    /// timestamps from filenames. E.g. `radar_(?P<timestamp>\d{8}T\d{4}Z)\.tif`
    pub filename_pattern: String,
    /// chrono strftime format for parsing the captured timestamp string.
    /// E.g. `%Y%m%dT%H%MZ`
    pub timestamp_format: String,
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
}

fn default_tile_cache_mb() -> u64 {
    64
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
        let content = std::fs::read_to_string(path)
            .map_err(|e| crate::error::DataServerError::Config(format!("Failed to read {path}: {e}")))?;
        toml::from_str(&content)
            .map_err(|e| crate::error::DataServerError::Config(format!("Failed to parse config: {e}")))
    }
}
