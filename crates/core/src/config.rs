use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub server: ServerSettings,
    #[serde(default)]
    pub collections: Vec<CollectionConfig>,
    /// Reusable named style bundles referenced from collection configs via
    /// `[wms] style_bundle = "..."`. Each bundle defines one default style
    /// plus zero or more named extras that apply to every collection referencing it.
    #[serde(default)]
    pub style_bundles: Vec<StyleBundle>,
}

#[derive(Debug, Deserialize)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    /// External base URL for generating absolute links (e.g. "https://api.example.com").
    /// If not set, defaults to "http://{host}:{port}".
    pub base_url: Option<String>,
    /// Bearer token for admin endpoints (e.g. /admin/collections/reload).
    /// Can also be set via `ADMIN_TOKEN` env var (takes priority).
    /// If neither is set, admin endpoints are disabled (return 403).
    pub admin_token: Option<String>,
    /// Directory containing per-collection `.toml` config files.
    /// Resolved relative to the parent directory of the main config file.
    /// Each file defines one collection using `CollectionConfig` fields directly.
    pub collections_dir: Option<String>,
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
    /// QueryData-specific configuration. Required when engine_type = "querydata".
    pub querydata: Option<QueryDataConfig>,
    /// GRIB-specific configuration. Required when engine_type = "grib".
    pub grib: Option<GribConfig>,
    /// WMS map rendering configuration. Required when apis contains "wms".
    pub wms: Option<WmsConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WmsConfig {
    /// Reference to a named bundle declared under top-level `[[style_bundles]]`.
    /// When set, the bundle's default + extras provide all styles for this
    /// collection and the other fields below (colormap/color_stops/styles/
    /// parameters/min/max) must be absent — mixing is rejected at config load.
    pub style_bundle: Option<String>,
    /// Built-in colormap name for the default style (e.g., "radar_dbz", "viridis").
    /// Ignored if color_stops are provided. Falls back to "viridis" if not set.
    pub colormap: Option<String>,
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
    /// Per-parameter default colormap overrides. For multi-parameter engines,
    /// each parameter layer can have its own default colormap and range.
    /// Parameters not listed here use the top-level `colormap`/`min`/`max`.
    #[serde(default)]
    pub parameters: Vec<WmsParameterConfig>,
    /// Rendered image cache size in MB. Default: 128.
    #[serde(default = "default_rendered_cache_mb")]
    pub rendered_cache_mb: u64,
}

/// Per-parameter default colormap configuration for WMS.
#[derive(Debug, Clone, Deserialize)]
pub struct WmsParameterConfig {
    /// Parameter short name (e.g., "2t", "msl"). Matched via param_index_by_name().
    pub name: String,
    /// Built-in colormap name for this parameter's default style.
    pub colormap: Option<String>,
    /// Custom color stops (overrides colormap).
    #[serde(default)]
    pub color_stops: Vec<ColorStop>,
    /// Minimum value for this parameter's colormap range.
    pub min: Option<f64>,
    /// Maximum value for this parameter's colormap range.
    pub max: Option<f64>,
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
    /// Data parameter to render for this style. For multi-parameter engines
    /// (e.g., querydata), selects which parameter's data is returned.
    /// If not set, the engine's default parameter is used.
    pub parameter: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ColorStop {
    pub value: f64,
    /// Color in "#RRGGBB" or "#RRGGBBAA" hex format.
    pub color: String,
}

fn default_rendered_cache_mb() -> u64 {
    512
}

/// A reusable named style bundle declared at top level of `config.toml` and
/// referenced by collections via `[wms] style_bundle = "..."`. Replaces the
/// per-collection `colormap` + `[[wms.styles]]` block when many collections
/// share the same set of styles (e.g. radar collections that all offer the
/// same four dBZ palettes).
#[derive(Debug, Clone, Deserialize)]
pub struct StyleBundle {
    /// Unique identifier referenced from `WmsConfig::style_bundle`.
    pub id: String,
    /// The default style (served when no STYLES= is given, or STYLES=default).
    pub default: StyleBundleDefault,
    /// Named extras — each becomes an additional WMS style for every
    /// collection that references this bundle.
    #[serde(default)]
    pub extras: Vec<StyleBundleExtra>,
}

/// Default style inside a `StyleBundle`. Same fields as a `WmsConfig` default
/// style, minus references and per-parameter bits.
#[derive(Debug, Clone, Deserialize)]
pub struct StyleBundleDefault {
    pub colormap: Option<String>,
    #[serde(default)]
    pub color_stops: Vec<ColorStop>,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

/// Named extra style inside a `StyleBundle`. Mirrors `WmsStyle` — the fields
/// are intentionally identical so bundle expansion maps 1:1 to the existing
/// `StyleInfo` builder path.
#[derive(Debug, Clone, Deserialize)]
pub struct StyleBundleExtra {
    pub name: String,
    pub title: Option<String>,
    pub colormap: Option<String>,
    #[serde(default)]
    pub color_stops: Vec<ColorStop>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub parameter: Option<String>,
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

    /// STAC API items endpoint URL. Mutually exclusive with `data_path` and
    /// `endpoint+bucket`. E.g. `"https://api.example.com/collections/radar/items"`
    pub stac_url: Option<String>,
    /// Asset key to extract from STAC items. Default: "data".
    #[serde(default = "default_stac_asset_key")]
    pub stac_asset_key: String,
    /// Required SSRF protection: list of allowed URL prefixes for asset URLs.
    /// E.g. `["https://thredds.met.no/"]`. Required when `stac_url` is set.
    pub stac_asset_allowlist: Option<Vec<String>>,

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

fn default_stac_asset_key() -> String {
    "data".to_string()
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

#[derive(Debug, Clone, Deserialize)]
pub struct QueryDataConfig {
    /// Parameter to expose for WMS/Maps rendering. Must match a parameter name
    /// in the querydata file. If not set, the first parameter is used.
    pub wms_parameter: Option<String>,
    /// Directory poll interval in seconds. Default: 30.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
}

fn default_grib_poll_interval() -> u64 {
    300
}

fn default_grid_cache_mb() -> u64 {
    256
}

#[derive(Debug, Clone, Deserialize)]
pub struct GribConfig {
    /// S3-compatible endpoint URL, e.g. "https://s3.amazonaws.com"
    pub endpoint: Option<String>,
    /// S3 bucket name. Required when endpoint is set.
    pub bucket: Option<String>,
    /// Prefix pattern with strftime templates, e.g. "%Y%m%d/00z/ifs/0p25/oper/"
    pub prefix_pattern: String,
    /// Suffix for index files. Default: ".index"
    pub index_suffix: Option<String>,
    /// Suffix for GRIB data files. Default: ".grib2"
    pub data_suffix: Option<String>,
    /// Poll interval in seconds. Default: 300 (5 min)
    #[serde(default = "default_grib_poll_interval")]
    pub poll_interval_secs: u64,
    /// Maximum number of forecast runs to retain. Default: 4
    pub max_runs: Option<usize>,
    /// Time window for file selection (ISO 8601 duration)
    pub time_window: Option<String>,
    /// Optional parameter filter — only expose these params. Default: all.
    pub parameters: Option<Vec<String>>,
    /// Grid cache size in MB. Default: 256.
    #[serde(default = "default_grid_cache_mb")]
    pub grid_cache_mb: u64,
    /// Model run hours to poll. Default: all (00, 06, 12, 18)
    pub run_hours: Option<Vec<u32>>,
    /// Index file format: "ecmwf-json" (default) or "wgrib2".
    /// ECMWF open data ships JSON-lines index files; NOAA GFS ships
    /// wgrib2 colon-separated text index files.
    pub index_format: Option<String>,
    /// Optional substring that every matching index filename must contain.
    /// Used to narrow down S3 listings when the directory holds multiple
    /// product variants sharing the index suffix. For example, GFS atmos
    /// directories contain pgrb2.0p25, pgrb2.0p50, pgrb2b, goessimpgrb2,
    /// etc. — all ending in `.idx`. Set `filename_contains = "pgrb2.0p25"`
    /// to keep only the 0.25-degree forecast files.
    pub filename_contains: Option<String>,
}

impl ServerConfig {
    /// Load config from a TOML file. If `collections_dir` is set, also loads
    /// per-file collection configs from that directory. Returns the config and
    /// a list of warning messages for the caller to log.
    pub fn from_file(path: &str) -> Result<(Self, Vec<String>), crate::error::DataServerError> {
        let config_path = std::path::Path::new(path);
        let content = std::fs::read_to_string(config_path).map_err(|e| {
            crate::error::DataServerError::Config(format!("Failed to read {path}: {e}"))
        })?;
        let mut config: Self = toml::from_str(&content).map_err(|e| {
            crate::error::DataServerError::Config(format!("Failed to parse config: {e}"))
        })?;

        let mut warnings = Vec::new();

        // Load per-file collection configs from collections_dir
        if let Some(ref dir) = config.server.collections_dir {
            let config_parent = config_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            let collections_dir = config_parent.join(dir);
            let collections_dir = collections_dir.canonicalize().map_err(|e| {
                crate::error::DataServerError::Config(format!(
                    "collections_dir '{}': {e}",
                    collections_dir.display()
                ))
            })?;

            let (dir_collections, dir_warnings) = Self::load_collections_dir(&collections_dir)?;
            warnings.extend(dir_warnings);
            config.collections.extend(dir_collections);
        }

        config.validate()?;
        Ok((config, warnings))
    }

    /// Load all `.toml` files from a directory, each containing one `CollectionConfig`.
    /// Returns the collections and a list of warning messages for the caller to log.
    pub fn load_collections_dir(
        dir: &std::path::Path,
    ) -> Result<(Vec<CollectionConfig>, Vec<String>), crate::error::DataServerError> {
        let entries = std::fs::read_dir(dir).map_err(|e| {
            crate::error::DataServerError::Config(format!(
                "collections_dir '{}': {e}",
                dir.display()
            ))
        })?;

        let mut files: Vec<std::path::PathBuf> = entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("toml") && path.is_file() {
                    Some(path)
                } else {
                    None
                }
            })
            .collect();

        // Deterministic byte-order sort by filename
        files.sort_unstable();

        let mut warnings = Vec::new();

        if files.is_empty() {
            warnings.push(format!(
                "collections_dir '{}' contains no .toml files",
                dir.display()
            ));
        }

        let mut collections = Vec::new();
        for file_path in &files {
            let filename = file_path.file_name().unwrap_or_default().to_string_lossy();

            let content = std::fs::read_to_string(file_path).map_err(|e| {
                crate::error::DataServerError::Config(format!("Failed to read {filename}: {e}"))
            })?;

            let collection: CollectionConfig = toml::from_str(&content).map_err(|e| {
                crate::error::DataServerError::Config(format!("Failed to parse {filename}: {e}"))
            })?;

            // Warn if filename stem differs from collection id
            let stem = file_path.file_stem().unwrap_or_default().to_string_lossy();
            if stem != collection.id {
                warnings.push(format!(
                    "{filename}: filename stem '{stem}' differs from collection id '{}'",
                    collection.id
                ));
            }

            collections.push(collection);
        }

        Ok((collections, warnings))
    }

    /// Validate configuration for common errors before starting the server.
    pub fn validate(&self) -> Result<(), crate::error::DataServerError> {
        // Check for duplicate style_bundle IDs + validate each bundle's extras.
        let mut bundle_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for bundle in &self.style_bundles {
            if bundle.id.is_empty() {
                return Err(crate::error::DataServerError::Config(
                    "Style bundle has an empty 'id' field".to_string(),
                ));
            }
            if !bundle_ids.insert(bundle.id.as_str()) {
                return Err(crate::error::DataServerError::Config(format!(
                    "Duplicate style_bundle ID '{}'",
                    bundle.id
                )));
            }
            // Extras must have non-empty, non-"default" names. "default" is
            // reserved for the bundle's default style; an extra using that
            // name would silently overwrite it in admin::build_styles.
            for extra in &bundle.extras {
                if extra.name.is_empty() {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Style bundle '{}': extra has an empty 'name' field",
                        bundle.id
                    )));
                }
                if extra.name == "default" {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Style bundle '{}': extra cannot be named 'default' \
                         (reserved for the bundle's default style)",
                        bundle.id
                    )));
                }
            }
        }

        // Check for duplicate collection IDs
        let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for (i, collection) in self.collections.iter().enumerate() {
            if let Some(&prev) = seen.get(collection.id.as_str()) {
                return Err(crate::error::DataServerError::Config(format!(
                    "Duplicate collection ID '{}': defined at collection index {prev} and {i}",
                    collection.id
                )));
            }
            seen.insert(&collection.id, i);
        }

        for collection in &self.collections {
            let id = &collection.id;

            if id.is_empty() {
                return Err(crate::error::DataServerError::Config(
                    "Collection has an empty 'id' field".to_string(),
                ));
            }

            // GeoTIFF engine requires geotiff config section
            if collection.engine_type == "geotiff" && collection.geotiff.is_none() {
                return Err(crate::error::DataServerError::Config(format!(
                    "Collection '{id}': engine_type 'geotiff' requires a [collections.geotiff] config section"
                )));
            }

            // GeoTIFF poll_interval_secs must be > 0
            if let Some(geotiff) = &collection.geotiff {
                if geotiff.poll_interval_secs == 0 {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Collection '{id}': poll_interval_secs must be > 0"
                    )));
                }
            }

            // GRIB engine requires grib config section
            if collection.engine_type == "grib" && collection.grib.is_none() {
                return Err(crate::error::DataServerError::Config(format!(
                    "Collection '{id}': engine_type 'grib' requires a [collections.grib] config section"
                )));
            }

            // GRIB poll_interval_secs must be > 0
            if let Some(grib) = &collection.grib {
                if grib.poll_interval_secs == 0 {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Collection '{id}': grib poll_interval_secs must be > 0"
                    )));
                }

                // Validate index_format
                if let Some(fmt) = grib.index_format.as_deref() {
                    if fmt != "ecmwf-json" && fmt != "wgrib2" {
                        return Err(crate::error::DataServerError::Config(format!(
                            "Collection '{id}': invalid grib index_format '{fmt}', \
                             expected 'ecmwf-json' or 'wgrib2'"
                        )));
                    }
                }
            }

            // style_bundle: reference must resolve, must not mix with inline WMS style fields
            if let Some(wms) = &collection.wms {
                if let Some(bundle_ref) = &wms.style_bundle {
                    if !bundle_ids.contains(bundle_ref.as_str()) {
                        return Err(crate::error::DataServerError::Config(format!(
                            "Collection '{id}': style_bundle '{bundle_ref}' is not defined \
                             in [[style_bundles]]"
                        )));
                    }
                    if wms.colormap.is_some()
                        || !wms.color_stops.is_empty()
                        || !wms.styles.is_empty()
                        || !wms.parameters.is_empty()
                        || wms.min.is_some()
                        || wms.max.is_some()
                    {
                        return Err(crate::error::DataServerError::Config(format!(
                            "Collection '{id}': style_bundle cannot be combined with inline \
                             colormap/color_stops/min/max/styles/parameters in [wms]"
                        )));
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn minimal_collection_toml(id: &str) -> String {
        format!(
            r#"
id = "{id}"
title = "Test"
description = "Test collection"
"#
        )
    }

    fn write_config(dir: &std::path::Path, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn from_file_without_collections_dir() {
        let tmp = TempDir::new().unwrap();
        let config_toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "test"
title = "Test"
description = "A test"
"#;
        let path = write_config(tmp.path(), "config.toml", config_toml);
        let (config, warnings) = ServerConfig::from_file(path.to_str().unwrap()).unwrap();
        assert_eq!(config.collections.len(), 1);
        assert_eq!(config.collections[0].id, "test");
        assert!(warnings.is_empty());
    }

    #[test]
    fn from_file_with_collections_dir() {
        let tmp = TempDir::new().unwrap();
        let collections_dir = tmp.path().join("collections.d");
        fs::create_dir(&collections_dir).unwrap();

        let config_toml = r#"
[server]
host = "127.0.0.1"
port = 8000
collections_dir = "collections.d"

[[collections]]
id = "inline"
title = "Inline"
description = "Inline collection"
"#;
        write_config(tmp.path(), "config.toml", config_toml);
        write_config(
            &collections_dir,
            "alpha.toml",
            &minimal_collection_toml("alpha"),
        );
        write_config(
            &collections_dir,
            "beta.toml",
            &minimal_collection_toml("beta"),
        );

        let path = tmp.path().join("config.toml");
        let (config, _warnings) = ServerConfig::from_file(path.to_str().unwrap()).unwrap();

        assert_eq!(config.collections.len(), 3);
        assert_eq!(config.collections[0].id, "inline");
        // Directory collections sorted alphabetically
        assert_eq!(config.collections[1].id, "alpha");
        assert_eq!(config.collections[2].id, "beta");
    }

    #[test]
    fn collections_dir_only_no_inline() {
        let tmp = TempDir::new().unwrap();
        let collections_dir = tmp.path().join("collections.d");
        fs::create_dir(&collections_dir).unwrap();

        let config_toml = r#"
[server]
host = "127.0.0.1"
port = 8000
collections_dir = "collections.d"
"#;
        write_config(tmp.path(), "config.toml", config_toml);
        write_config(
            &collections_dir,
            "radar.toml",
            &minimal_collection_toml("radar"),
        );

        let path = tmp.path().join("config.toml");
        let (config, _) = ServerConfig::from_file(path.to_str().unwrap()).unwrap();
        assert_eq!(config.collections.len(), 1);
        assert_eq!(config.collections[0].id, "radar");
    }

    #[test]
    fn duplicate_id_across_inline_and_dir() {
        let tmp = TempDir::new().unwrap();
        let collections_dir = tmp.path().join("collections.d");
        fs::create_dir(&collections_dir).unwrap();

        let config_toml = r#"
[server]
host = "127.0.0.1"
port = 8000
collections_dir = "collections.d"

[[collections]]
id = "radar"
title = "Inline Radar"
description = "Inline"
"#;
        write_config(tmp.path(), "config.toml", config_toml);
        write_config(
            &collections_dir,
            "radar.toml",
            &minimal_collection_toml("radar"),
        );

        let path = tmp.path().join("config.toml");
        let result = ServerConfig::from_file(path.to_str().unwrap());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Duplicate collection ID 'radar'"),
            "got: {err}"
        );
    }

    #[test]
    fn duplicate_id_across_dir_files() {
        let tmp = TempDir::new().unwrap();
        let collections_dir = tmp.path().join("collections.d");
        fs::create_dir(&collections_dir).unwrap();

        let config_toml = r#"
[server]
host = "127.0.0.1"
port = 8000
collections_dir = "collections.d"
"#;
        write_config(tmp.path(), "config.toml", config_toml);
        write_config(
            &collections_dir,
            "a.toml",
            &minimal_collection_toml("same-id"),
        );
        write_config(
            &collections_dir,
            "b.toml",
            &minimal_collection_toml("same-id"),
        );

        let path = tmp.path().join("config.toml");
        let result = ServerConfig::from_file(path.to_str().unwrap());
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Duplicate collection ID 'same-id'"));
    }

    #[test]
    fn non_toml_files_ignored() {
        let tmp = TempDir::new().unwrap();
        let collections_dir = tmp.path().join("collections.d");
        fs::create_dir(&collections_dir).unwrap();

        let config_toml = r#"
[server]
host = "127.0.0.1"
port = 8000
collections_dir = "collections.d"
"#;
        write_config(tmp.path(), "config.toml", config_toml);
        write_config(
            &collections_dir,
            "radar.toml",
            &minimal_collection_toml("radar"),
        );
        write_config(&collections_dir, "radar.toml.disabled", "invalid toml {{{}");
        write_config(&collections_dir, "README.md", "# Collections");
        write_config(&collections_dir, "backup.bak", "junk");

        let path = tmp.path().join("config.toml");
        let (config, _) = ServerConfig::from_file(path.to_str().unwrap()).unwrap();
        assert_eq!(config.collections.len(), 1);
        assert_eq!(config.collections[0].id, "radar");
    }

    #[test]
    fn malformed_toml_in_dir() {
        let tmp = TempDir::new().unwrap();
        let collections_dir = tmp.path().join("collections.d");
        fs::create_dir(&collections_dir).unwrap();

        let config_toml = r#"
[server]
host = "127.0.0.1"
port = 8000
collections_dir = "collections.d"
"#;
        write_config(tmp.path(), "config.toml", config_toml);
        write_config(&collections_dir, "bad.toml", "this is not valid {{ toml");

        let path = tmp.path().join("config.toml");
        let result = ServerConfig::from_file(path.to_str().unwrap());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("bad.toml"),
            "error should name the file, got: {err}"
        );
    }

    #[test]
    fn missing_collections_dir_is_error() {
        let tmp = TempDir::new().unwrap();
        let config_toml = r#"
[server]
host = "127.0.0.1"
port = 8000
collections_dir = "nonexistent"
"#;
        write_config(tmp.path(), "config.toml", config_toml);

        let path = tmp.path().join("config.toml");
        let result = ServerConfig::from_file(path.to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nonexistent"));
    }

    #[test]
    fn empty_dir_warns() {
        let tmp = TempDir::new().unwrap();
        let collections_dir = tmp.path().join("collections.d");
        fs::create_dir(&collections_dir).unwrap();

        let config_toml = r#"
[server]
host = "127.0.0.1"
port = 8000
collections_dir = "collections.d"
"#;
        write_config(tmp.path(), "config.toml", config_toml);

        let path = tmp.path().join("config.toml");
        let (config, warnings) = ServerConfig::from_file(path.to_str().unwrap()).unwrap();
        assert!(config.collections.is_empty());
        assert!(warnings.iter().any(|w| w.contains("no .toml files")));
    }

    #[test]
    fn filename_stem_mismatch_warns() {
        let tmp = TempDir::new().unwrap();
        let collections_dir = tmp.path().join("collections.d");
        fs::create_dir(&collections_dir).unwrap();

        let config_toml = r#"
[server]
host = "127.0.0.1"
port = 8000
collections_dir = "collections.d"
"#;
        write_config(tmp.path(), "config.toml", config_toml);
        // File named "radar.toml" but id is "weather"
        write_config(
            &collections_dir,
            "radar.toml",
            &minimal_collection_toml("weather"),
        );

        let path = tmp.path().join("config.toml");
        let (config, warnings) = ServerConfig::from_file(path.to_str().unwrap()).unwrap();
        assert_eq!(config.collections.len(), 1);
        assert_eq!(config.collections[0].id, "weather");
        assert!(warnings
            .iter()
            .any(|w| w.contains("differs from collection id")));
    }

    #[test]
    fn alphabetical_ordering() {
        let tmp = TempDir::new().unwrap();
        let collections_dir = tmp.path().join("collections.d");
        fs::create_dir(&collections_dir).unwrap();

        let config_toml = r#"
[server]
host = "127.0.0.1"
port = 8000
collections_dir = "collections.d"
"#;
        write_config(tmp.path(), "config.toml", config_toml);
        write_config(
            &collections_dir,
            "02-beta.toml",
            &minimal_collection_toml("beta"),
        );
        write_config(
            &collections_dir,
            "01-alpha.toml",
            &minimal_collection_toml("alpha"),
        );
        write_config(
            &collections_dir,
            "03-gamma.toml",
            &minimal_collection_toml("gamma"),
        );

        let path = tmp.path().join("config.toml");
        let (config, _) = ServerConfig::from_file(path.to_str().unwrap()).unwrap();
        let ids: Vec<&str> = config.collections.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "beta", "gamma"]);
    }

    // ---- style_bundles ------------------------------------------------------

    fn radar_bundle_toml() -> &'static str {
        r#"
[[style_bundles]]
id = "radar_multi"

[style_bundles.default]
colormap = "radar_bookbinder"

[[style_bundles.extras]]
name = "radar_dbz"
title = "MeteoCore Radar"
colormap = "radar_dbz"

[[style_bundles.extras]]
name = "radar_fmi"
title = "FMI Radar"
colormap = "radar_fmi"
"#
    }

    #[test]
    fn style_bundle_parses_and_collection_reference_resolves() {
        let tmp = TempDir::new().unwrap();
        let config_toml = format!(
            r#"
[server]
host = "127.0.0.1"
port = 8000

{bundle}

[[collections]]
id = "radar-dwd"
title = "DWD"
description = "DWD"
engine_type = "geotiff"

[collections.geotiff]
filename_template = "radar_%Y%m%dT%H%MZ.tif"
parameter = "reflectivity"
unit = "dBZ"
data_path = "/tmp"

[collections.wms]
style_bundle = "radar_multi"
"#,
            bundle = radar_bundle_toml()
        );
        let path = write_config(tmp.path(), "config.toml", &config_toml);
        let (config, _) = ServerConfig::from_file(path.to_str().unwrap()).unwrap();
        assert_eq!(config.style_bundles.len(), 1);
        assert_eq!(config.style_bundles[0].id, "radar_multi");
        assert_eq!(config.style_bundles[0].extras.len(), 2);
        assert_eq!(
            config.collections[0]
                .wms
                .as_ref()
                .unwrap()
                .style_bundle
                .as_deref(),
            Some("radar_multi")
        );
    }

    #[test]
    fn duplicate_style_bundle_id_rejected() {
        let tmp = TempDir::new().unwrap();
        let config_toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[style_bundles]]
id = "dup"
[style_bundles.default]
colormap = "viridis"

[[style_bundles]]
id = "dup"
[style_bundles.default]
colormap = "grayscale"
"#;
        let path = write_config(tmp.path(), "config.toml", config_toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Duplicate style_bundle ID 'dup'"),
            "got: {err}"
        );
    }

    #[test]
    fn unknown_style_bundle_reference_rejected() {
        let tmp = TempDir::new().unwrap();
        let config_toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "radar-dwd"
title = "DWD"
description = "DWD"
engine_type = "geotiff"

[collections.geotiff]
filename_template = "radar_%Y%m%dT%H%MZ.tif"
parameter = "reflectivity"
unit = "dBZ"
data_path = "/tmp"

[collections.wms]
style_bundle = "missing"
"#;
        let path = write_config(tmp.path(), "config.toml", config_toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("style_bundle 'missing' is not defined"),
            "got: {err}"
        );
    }

    #[test]
    fn style_bundle_cannot_be_mixed_with_inline_wms_fields() {
        let tmp = TempDir::new().unwrap();
        let config_toml = format!(
            r#"
[server]
host = "127.0.0.1"
port = 8000

{bundle}

[[collections]]
id = "radar-dwd"
title = "DWD"
description = "DWD"
engine_type = "geotiff"

[collections.geotiff]
filename_template = "radar_%Y%m%dT%H%MZ.tif"
parameter = "reflectivity"
unit = "dBZ"
data_path = "/tmp"

[collections.wms]
style_bundle = "radar_multi"
colormap = "viridis"
"#,
            bundle = radar_bundle_toml()
        );
        let path = write_config(tmp.path(), "config.toml", &config_toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("style_bundle cannot be combined with inline"),
            "got: {err}"
        );
    }

    #[test]
    fn style_bundle_cannot_be_mixed_with_inline_styles_array() {
        let tmp = TempDir::new().unwrap();
        let config_toml = format!(
            r#"
[server]
host = "127.0.0.1"
port = 8000

{bundle}

[[collections]]
id = "radar-dwd"
title = "DWD"
description = "DWD"
engine_type = "geotiff"

[collections.geotiff]
filename_template = "radar_%Y%m%dT%H%MZ.tif"
parameter = "reflectivity"
unit = "dBZ"
data_path = "/tmp"

[collections.wms]
style_bundle = "radar_multi"

[[collections.wms.styles]]
name = "extra"
colormap = "viridis"
"#,
            bundle = radar_bundle_toml()
        );
        let path = write_config(tmp.path(), "config.toml", &config_toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("style_bundle cannot be combined with inline"),
            "got: {err}"
        );
    }

    #[test]
    fn empty_style_bundle_id_rejected() {
        let tmp = TempDir::new().unwrap();
        let config_toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[style_bundles]]
id = ""
[style_bundles.default]
colormap = "viridis"
"#;
        let path = write_config(tmp.path(), "config.toml", config_toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Style bundle has an empty 'id' field"),
            "got: {err}"
        );
    }

    #[test]
    fn extra_with_empty_name_rejected() {
        let tmp = TempDir::new().unwrap();
        let config_toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[style_bundles]]
id = "radar_multi"
[style_bundles.default]
colormap = "radar_dbz"

[[style_bundles.extras]]
name = ""
colormap = "radar_fmi"
"#;
        let path = write_config(tmp.path(), "config.toml", config_toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Style bundle 'radar_multi': extra has an empty 'name' field"),
            "got: {err}"
        );
    }

    #[test]
    fn extra_named_default_rejected() {
        // Without this check, an extra named "default" would silently clobber
        // the bundle's default style when build_styles inserts the extras.
        let tmp = TempDir::new().unwrap();
        let config_toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[style_bundles]]
id = "radar_multi"
[style_bundles.default]
colormap = "radar_dbz"

[[style_bundles.extras]]
name = "default"
colormap = "radar_fmi"
"#;
        let path = write_config(tmp.path(), "config.toml", config_toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("extra cannot be named 'default'"),
            "got: {err}"
        );
    }

    #[test]
    fn wms_without_colormap_or_bundle_defaults_to_none() {
        // Verifies the Option<String> migration: omitting `colormap` no longer
        // forces a default at parse time — the default "viridis" is applied
        // later by build_colormap_from_wms_config. Previously this field was a
        // required String with a serde default.
        let tmp = TempDir::new().unwrap();
        let config_toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "x"
title = "X"
description = "X"

[collections.wms]
"#;
        let path = write_config(tmp.path(), "config.toml", config_toml);
        let (config, _) = ServerConfig::from_file(path.to_str().unwrap()).unwrap();
        let wms = config.collections[0].wms.as_ref().unwrap();
        assert_eq!(wms.colormap, None);
        assert_eq!(wms.style_bundle, None);
    }
}
