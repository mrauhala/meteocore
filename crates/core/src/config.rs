use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub server: ServerSettings,
    #[serde(default)]
    pub collections: Vec<CollectionConfig>,
    /// Shared style bundles referenced by collections via `[wms] style_bundle`.
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
    /// Size in MB of the global Web Mercator meta-tile (decoded-RGBA) cache
    /// (#202). A single server-wide cache, not per-collection. Currently
    /// consumed only by the WMS GetMap path; api-maps/api-tiles still render
    /// directly and would share this same cache once meta-tiling is extended to
    /// them (follow-up). Default: 1024. Set to `0` to disable meta-tiling
    /// entirely (the EPSG:3857 GetMap path reverts to a direct single-shot
    /// render), reversible via config reload.
    #[serde(default = "default_metatile_cache_mb")]
    pub metatile_cache_mb: u64,
    /// Watch `collections_dir` for changes and auto-reload (add/remove/update
    /// collections) when `.toml` files are added, edited, or removed — no manual
    /// `POST /admin/collections/reload` needed. Opt-in; default `false`. Has no
    /// effect unless `collections_dir` is also set.
    #[serde(default)]
    pub watch_collections_dir: bool,
    /// Debounce window (milliseconds) for the `collections_dir` watcher: rapid
    /// or multi-event file changes (an editor's write-then-rename) within this
    /// window coalesce into a single reload. Default: 500.
    #[serde(default = "default_watch_debounce_ms")]
    pub watch_debounce_ms: u64,
    /// Trust reverse-proxy forwarding headers (`Forwarded`, `X-Forwarded-Proto`,
    /// `X-Forwarded-Host`, `X-Forwarded-Port`) to derive each request's
    /// absolute self-link base URL (#12). Default `false`. Enable ONLY when the
    /// server sits behind a trusted proxy that sets/overwrites these headers —
    /// otherwise a client could spoof them. When `false`, links use the static
    /// base URL (`BASE_URL` env > `[server] base_url` > `http://{host}:{port}`).
    #[serde(default)]
    pub trust_proxy_headers: bool,
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

impl ServerConfig {
    /// Build a config for the no-config-file boot path: the server is started
    /// without a `config.toml` (e.g. to be pointed at a directory with no
    /// config — see #411 auto-collections). Host defaults to loopback and port
    /// to 8000; when the port is not pinned by config or `--port`, the binary
    /// auto-scans upward from there for the first free port. Collections and
    /// style bundles start empty and can be added via reload.
    pub fn default_for_auto() -> Self {
        ServerConfig {
            server: ServerSettings {
                host: "127.0.0.1".to_string(),
                port: 8000,
                base_url: None,
                admin_token: None,
                collections_dir: None,
                metatile_cache_mb: default_metatile_cache_mb(),
                watch_collections_dir: false,
                watch_debounce_ms: default_watch_debounce_ms(),
                trust_proxy_headers: false,
            },
            collections: Vec::new(),
            style_bundles: Vec::new(),
        }
    }
}

/// Deserialize a keyword list, trimming surrounding whitespace from each entry
/// and dropping exact duplicates (first occurrence wins, order preserved) so
/// `["radar", "radar"]` can't produce doubled chips / `<Keyword>` elements / JSON
/// entries. A padded keyword (e.g. `" radar "`) is trimmed first so the dedup is
/// on the clean value; an all-whitespace entry trims to `""`, which
/// `CollectionConfig::validate` then rejects as a vacuous keyword.
fn de_trimmed_keywords<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<String>::deserialize(d)?;
    let mut seen = std::collections::HashSet::new();
    Ok(raw
        .into_iter()
        .map(|k| k.trim().to_string())
        .filter(|k| seen.insert(k.clone()))
        .collect())
}

/// Deserialize a string, trimming surrounding whitespace — so a padded value
/// (e.g. a license `title = "  CC-BY 4.0  "`) can't render with visible spaces
/// in the JSON link title, WMS `<Title>`, or HTML page.
fn de_trimmed_string<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(String::deserialize(d)?.trim().to_string())
}

/// Like [`de_trimmed_string`] but for an optional string (e.g. a license `url`).
fn de_trimmed_opt_string<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(d)?.map(|s| s.trim().to_string()))
}

#[derive(Debug, Clone, Deserialize)]
pub struct CollectionConfig {
    pub id: String,
    pub title: String,
    pub description: String,
    /// Free-text keywords for discovery. Surfaced in the OGC API – Common
    /// collection description (`"keywords"`), the WMS `<KeywordList>`, the HTML
    /// collection cards, and matched by `/collections?q=`. Empty by default.
    /// Each entry is trimmed at load so padding can't leak into chips/`<Keyword>`.
    #[serde(default, deserialize_with = "de_trimmed_keywords")]
    pub keywords: Vec<String>,
    /// License for this collection's data. Surfaced as a `rel="license"` link in
    /// the JSON APIs, a `<Attribution>` element in WMS, and a link in the HTML
    /// cards. Absent by default.
    #[serde(default)]
    pub license: Option<LicenseConfig>,
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
    /// Zarr-specific configuration. Required when engine_type = "zarr".
    pub zarr: Option<ZarrConfig>,
    /// ODIM_H5 radar-specific configuration. Required when engine_type = "odim".
    pub odim: Option<OdimConfig>,
    /// CAP alert-specific configuration. Required when engine_type = "cap".
    pub cap: Option<CapConfig>,
    /// WMS map rendering configuration. Required when apis contains "wms".
    pub wms: Option<WmsConfig>,
    /// PostGIS-specific configuration. Required when engine_type = "postgis".
    pub postgis: Option<PostgisConfig>,
    /// Preview-SPA-specific tuning (e.g. bound the time slider). Optional.
    pub preview: Option<PreviewConfig>,
}

/// License metadata for a collection's data.
///
/// Rendered as an OGC `rel="license"` link in the JSON APIs (href =
/// [`resolved_url`](Self::resolved_url), title = `title`) and a WMS
/// `<Attribution>` element. `title` is required (the human-readable name, e.g.
/// `"CC-BY 4.0"`, or an SPDX id like `"CC-BY-4.0"`); `url` is optional and, when
/// omitted, is synthesized from `title` if it parses as an SPDX identifier.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct LicenseConfig {
    /// Human-readable license name or SPDX identifier (the link title).
    /// Trimmed at load so padding can't leak into the rendered title.
    #[serde(deserialize_with = "de_trimmed_string")]
    pub title: String,
    /// Explicit URL to the license text. When absent, [`resolved_url`] falls
    /// back to an `spdx.org` URL if `title` looks like an SPDX id. Trimmed at
    /// load (like `title`) so the raw field is always clean.
    ///
    /// [`resolved_url`]: Self::resolved_url
    #[serde(default, deserialize_with = "de_trimmed_opt_string")]
    pub url: Option<String>,
}

impl LicenseConfig {
    /// The license URL to advertise: the explicit `url` if set, else an
    /// `spdx.org` URL synthesized from `title` when it is a plausible SPDX id,
    /// else `None` (the caller emits a license link only when this is `Some`).
    ///
    /// Note: a deprecated `+`-suffix id (e.g. `GPL-2.0+`) is accepted by the
    /// shape check and synthesizes `https://spdx.org/licenses/GPL-2.0+.html`,
    /// which `spdx.org` serves only via a redirect to the canonical
    /// `-or-later` page. For a canonical `href`, set an explicit `url` or use
    /// the non-deprecated id (e.g. `GPL-2.0-or-later`).
    pub fn resolved_url(&self) -> Option<String> {
        if let Some(url) = &self.url {
            // Already trimmed at load by `de_trimmed_opt_string`.
            return Some(url.clone());
        }
        // SPDX ids are short tokens of [A-Za-z0-9.+-] (e.g. "CC-BY-4.0",
        // "Apache-2.0"). Only synthesize for that shape so a free-text title
        // like "All rights reserved" doesn't produce a bogus link.
        let t = self.title.trim();
        if !t.is_empty()
            && t.len() <= 64
            && t.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'))
        {
            Some(format!("https://spdx.org/licenses/{t}.html"))
        } else {
            None
        }
    }

    /// `(title, href)` for a license **link**, or `None` when no URL is
    /// resolvable (a link object / `<a href>` needs a target). Used for the
    /// JSON `rel="license"` link, which cannot exist without an `href`.
    pub fn card_link(&self) -> Option<(String, String)> {
        self.resolved_url().map(|url| (self.title.clone(), url))
    }

    /// `(title, href?)` for **display** contexts (HTML cards) that show the
    /// license name even when no URL resolves. Unlike [`card_link`](Self::card_link)
    /// this always yields the title; the href is `None` for a free-text license
    /// with no explicit `url` (rendered as plain text rather than a link).
    pub fn card_label(&self) -> (String, Option<String>) {
        (self.title.clone(), self.resolved_url())
    }
}

/// Preview-SPA tuning knobs. Only affects what `/preview/manifest.json`
/// emits — does NOT constrain the underlying engine.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PreviewConfig {
    /// ISO 8601 positive duration (e.g. `"PT12H"`, `"P1D"`). When set, the
    /// manifest's `temporal_extent.values` is filtered to entries within
    /// `[max(values) - duration, max(values)]`. Useful for STAC-backed
    /// collections whose archive spans years but whose useful slider range
    /// is the most recent few hours.
    pub time_window: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WmsConfig {
    /// Named bundle; mixing with inline colormap/styles/etc. is a config error.
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
    ///
    /// NOTE: like the meta-tile cache, this is actually a *global* shared cache,
    /// not per-collection; it lives here for backward compatibility. New global
    /// cache knobs (e.g. `[server] metatile_cache_mb`) go under `[server]`.
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

fn default_metatile_cache_mb() -> u64 {
    1024
}

fn default_watch_debounce_ms() -> u64 {
    500
}

/// Shared WMS style set: one default + zero or more named extras.
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

/// Default style inside a `StyleBundle`.
#[derive(Debug, Clone, Deserialize)]
pub struct StyleBundleDefault {
    pub colormap: Option<String>,
    #[serde(default)]
    pub color_stops: Vec<ColorStop>,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

/// Named extra style inside a `StyleBundle`.
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

fn default_querydata_max_runs() -> usize {
    4
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
    /// How many recent model runs (`.sqd` files, keyed by origin/analysis time)
    /// to retain and expose as OGC EDR instances / a WMS `reference_time`
    /// dimension. The newest run is always the default for un-pinned queries.
    /// Default: 4. Set to 1 to keep only the latest run (no model-run history).
    #[serde(default = "default_querydata_max_runs")]
    pub max_runs: usize,
}

impl QueryDataConfig {
    /// Defaults for an auto-discovered local `.sqd` directory (#411): no WMS
    /// parameter selected (the first is used for rendering), default poll/runs.
    pub fn auto_default() -> Self {
        QueryDataConfig {
            wms_parameter: None,
            poll_interval_secs: default_poll_interval(),
            max_runs: default_querydata_max_runs(),
        }
    }
}

fn default_cap_poll_interval() -> u64 {
    300
}

fn default_circle_segments() -> u32 {
    64
}

fn default_geocode_property() -> String {
    "code".to_string()
}

fn default_status_filter() -> Vec<String> {
    vec!["Actual".to_string()]
}

/// Configuration for the CAP (Common Alerting Protocol) v1.2 alert engine
/// (`engine_type = "cap"`).
///
/// The source is **exactly one** of a local directory of CAP `.xml` files
/// (`data_path`) or an Atom/RSS feed whose entries link to individual CAP
/// documents (`feed_url`, the MeteoAlarm / US-NWS pattern). Each alert area
/// becomes one OGC API Features feature and is rendered into the WMS/Maps/Tiles
/// alert layer as a severity-shaded polygon. See the "CAP Engine Notes" section
/// in `CLAUDE.md`.
#[derive(Debug, Clone, Deserialize)]
pub struct CapConfig {
    /// Local directory of CAP `.xml` files. Mutually exclusive with `feed_url`.
    #[serde(default, deserialize_with = "de_trimmed_opt_string")]
    pub data_path: Option<String>,
    /// Atom/RSS feed URL whose entries link to individual CAP documents.
    /// Mutually exclusive with `data_path`; must be `http(s)`.
    #[serde(default, deserialize_with = "de_trimmed_opt_string")]
    pub feed_url: Option<String>,
    /// Poll interval in seconds for re-scanning the directory / re-fetching the
    /// feed. Default: 300.
    #[serde(default = "default_cap_poll_interval")]
    pub poll_interval_secs: u64,
    /// Which `<info>` language to expose when an alert carries multiple
    /// translations (CAP `<info><language>`, a BCP 47 / RFC 3066 tag). When set,
    /// the matching `<info>` is preferred; when absent or unmatched, the first
    /// `<info>` is used. Empty string is rejected at config load.
    #[serde(default, deserialize_with = "de_trimmed_opt_string")]
    pub language: Option<String>,
    /// CAP `<status>` values to serve (case-insensitive). Default `["Actual"]`,
    /// dropping `Test`/`Exercise`/`Draft`/`System` alerts. An empty list serves
    /// every status.
    #[serde(default = "default_status_filter")]
    pub status_filter: Vec<String>,
    /// Validity window applied when an info has no `<expires>` — an ISO 8601
    /// positive duration (e.g. `"PT24H"`, `"P1D"`) added to the onset/effective
    /// time. When absent, a missing `<expires>` means open-ended (active until
    /// superseded).
    #[serde(default, deserialize_with = "de_trimmed_opt_string")]
    pub default_ttl: Option<String>,
    /// Number of polygon vertices used to approximate a CAP `<circle>` as an
    /// N-gon on the geodesic. Default: 64.
    #[serde(default = "default_circle_segments")]
    pub circle_segments: u32,
    /// Optional GeoJSON `FeatureCollection` mapping zone codes → polygons, used
    /// to give geometry to geocode-only areas (e.g. MeteoAlarm's EMMA_ID zones,
    /// which carry no inline `<polygon>`). Without it such areas have null
    /// geometry (listed in Features, absent from the map, no spatial extent).
    #[serde(default, deserialize_with = "de_trimmed_opt_string")]
    pub geocode_geometry: Option<String>,
    /// The GeoJSON feature property holding the zone code matched against CAP
    /// `<geocode>` values. Default `"code"`.
    #[serde(default = "default_geocode_property")]
    pub geocode_property: String,
    /// Restrict geocode resolution to CAP `<geocode>` entries with this
    /// `<valueName>` (e.g. `"EMMA_ID"`); unset resolves against any geocode value.
    #[serde(default, deserialize_with = "de_trimmed_opt_string")]
    pub geocode_value_name: Option<String>,
    /// SSRF guard for feed mode: extra URL **prefixes** an entry link may match
    /// to be fetched, *in addition to* the feed's own origin (which is always
    /// allowed). An entry link whose origin differs from the feed's and matches
    /// no prefix here is dropped with a WARN — so a compromised feed cannot pivot
    /// the server to `http://169.254.169.254/…` or any internal host. Mirrors the
    /// GeoTIFF STAC `stac_asset_allowlist`. Each entry must be an `http(s)` URL
    /// prefix **ending in `/`** (enforced at load) so a prefix can't widen — e.g.
    /// `https://cdn/cap/` must not also admit `https://cdn/cap-staging/`. Empty
    /// (default) ⇒ entry links must share the feed's origin.
    #[serde(default)]
    pub feed_allowlist: Vec<String>,
}

/// Configuration for the ODIM_H5 weather-radar engine
/// (`engine_type = "odim"`).
///
/// Phase 1 supports COMP (composite) reflectivity files from a local
/// directory or an S3-compatible bucket. STAC sources land in
/// Phase 2; PVOL polar-volume EDR trajectory queries land in Phase 3.
/// See [[project_odim_engine_plan]] for the full multi-phase roadmap.
#[derive(Debug, Clone, Deserialize)]
pub struct OdimConfig {
    /// Strftime filename template. ODIM files typically encode time
    /// in the filename (e.g. `"202503251200_radar_fi.h5"` or
    /// `"%Y%m%dT%H%M_polar_finland_anjalankoski.h5"`). Auto-derives
    /// the regex and timestamp format. Preferred over `filename_pattern`.
    pub filename_template: Option<String>,
    /// Explicit regex with named `(?P<timestamp>…)` capture group, for
    /// filenames `filename_template` can't express. Requires
    /// `timestamp_format`.
    pub filename_pattern: Option<String>,
    /// chrono strftime format for parsing `filename_pattern`'s
    /// timestamp capture.
    pub timestamp_format: Option<String>,

    /// Parameter name advertised to clients (e.g. `"reflectivity"`).
    /// Required for single-parameter `engine_type = "odim"` (COMP)
    /// collections. Unused — and may be omitted — by the multi-parameter
    /// `engine_type = "odim-volume"` (PVOL) engine, which auto-expands into
    /// one collection per radar site whose parameters are the bare ODIM
    /// quantities (`DBZH`, `VRADH`, …) read from the volume files.
    pub parameter: Option<String>,
    /// Unit of measurement (e.g. `"dBZ"`). Pure metadata: the engine
    /// does not convert between units. Required for `engine_type = "odim"`;
    /// optional for `engine_type = "odim-volume"`.
    pub unit: Option<String>,

    /// Override nodata sentinel. Takes precedence over the file's
    /// `/dataset1/data1/what/nodata` attribute. Useful when a
    /// producer ships files with a missing or mis-declared nodata
    /// value.
    pub nodata: Option<f64>,
    /// Override gain factor. Takes precedence over `/dataset1/data1/
    /// what/gain` (or root-level `/what/gain` for producers that
    /// place it there). `physical = raw * gain + offset`.
    pub gain: Option<f64>,
    /// Override offset. Same precedence rules as `gain`.
    pub offset: Option<f64>,

    /// Directory poll interval in seconds. Default: 30.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Maximum number of files to keep in the catalog (most recent by
    /// timestamp). Default: unbounded. Useful for sources where the
    /// directory may hold years of history.
    pub max_files: Option<usize>,

    /// S3-compatible endpoint URL (e.g. `https://s3.waw3-1.cloudferro.com`).
    /// When `endpoint` + `bucket` are set the engine reads from S3.
    /// Otherwise the source is the collection's `data_path`: an
    /// `http(s)://` URL selects an HTTP(S) object store (COMP only — the
    /// directory must be WebDAV/`PROPFIND`-listable; a plain Apache/nginx
    /// autoindex is not), and any other value is a local directory.
    pub endpoint: Option<String>,
    /// S3 bucket name. Required when `endpoint` is set.
    pub bucket: Option<String>,
    /// Object key prefix, optionally carrying strftime templates
    /// (e.g. `%Y/%m/%d/OPERA/COMP/`). Expanded per UTC date on every
    /// poll so the scan stays current across day boundaries. For an
    /// `http(s)://` `data_path` (COMP) it is appended under the URL path.
    pub prefix_pattern: Option<String>,
    /// ISO 8601 duration bounding which timesteps to keep, relative to
    /// now (e.g. `-PT12H` for the last 12 hours). Drives both prefix
    /// date expansion and timestamp filtering for S3/HTTP sources. When
    /// unset, the scan falls back to a fixed recent-days window.
    pub time_window: Option<String>,
    /// Discovery mode for an `http(s)://` `data_path` source (COMP only;
    /// ignored for local and S3 sources):
    /// - `"list"` (default) — enumerate the directory with WebDAV
    ///   `PROPFIND`. Works for listable HTTP stores (#286).
    /// - `"template"` — don't list; instead build candidate filenames
    ///   from `filename_template` + `cadence_secs` walking back from now,
    ///   and probe them with `HEAD`. For non-listable autoindex servers
    ///   such as DWD opendata (#287). Requires `filename_template`.
    pub discovery: Option<String>,
    /// Probe cadence in seconds for `discovery = "template"` — the spacing
    /// of candidate timestamps (e.g. `300` for a 5-minute radar feed).
    /// Required in template mode; ignored otherwise.
    pub cadence_secs: Option<u64>,
}

fn default_grib_poll_interval() -> u64 {
    // 10 minutes. NWP models typically publish a new run every 6 h (some less
    // often), with steps trickling in over ~1-2 h, so frequent polling mostly
    // re-lists unchanged runs. Override per collection via `poll_interval_secs`
    // for the rare model that updates more often.
    600
}

fn default_grid_cache_mb() -> u64 {
    256
}

#[derive(Debug, Clone, Deserialize)]
pub struct GribConfig {
    /// Local directory of `.grib2` + index sidecar files. Mutually exclusive
    /// with `endpoint`/`bucket` (S3). When set, the engine lists this directory
    /// directly for index files — `prefix_pattern`'s strftime/run-hour
    /// templating does not apply (it is used, if present, as a literal
    /// sub-prefix; default = the directory root). A remote URL
    /// (`s3://`/`http(s)://`) is also accepted here as a fixed-prefix source.
    pub data_path: Option<String>,
    /// S3-compatible endpoint URL, e.g. "https://s3.amazonaws.com"
    pub endpoint: Option<String>,
    /// S3 bucket name. Required when endpoint is set.
    pub bucket: Option<String>,
    /// Prefix pattern with strftime templates, e.g. "%Y%m%d/00z/ifs/0p25/oper/".
    /// Required for the remote (`endpoint`+`bucket`) source. Optional for a
    /// local `data_path`, where it is used (if present) as a *literal*
    /// sub-prefix under the directory — no strftime/date templating; default
    /// "" = the directory root.
    pub prefix_pattern: Option<String>,
    /// Suffix for index files. Default: ".index"
    pub index_suffix: Option<String>,
    /// Suffix for GRIB data files. Default: ".grib2"
    pub data_suffix: Option<String>,
    /// Poll interval in seconds. Default: 600 (10 min)
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

impl GribConfig {
    /// Defaults for an auto-discovered local directory of GRIB2 + index
    /// sidecars (#411). `data_path` is the directory; the index/data suffixes
    /// and index format come from what was found on disk.
    pub fn auto_local(
        data_path: String,
        index_suffix: String,
        data_suffix: String,
        index_format: Option<String>,
    ) -> Self {
        GribConfig {
            data_path: Some(data_path),
            endpoint: None,
            bucket: None,
            prefix_pattern: None,
            index_suffix: Some(index_suffix),
            data_suffix: Some(data_suffix),
            poll_interval_secs: default_grib_poll_interval(),
            max_runs: None,
            time_window: None,
            parameters: None,
            grid_cache_mb: default_grid_cache_mb(),
            run_hours: None,
            index_format,
            filename_contains: None,
        }
    }
}

fn default_zarr_poll_interval() -> u64 {
    300
}

fn default_zarr_cache_mb() -> u64 {
    256
}

/// Configuration for the Zarr engine (`engine_type = "zarr"`).
///
/// Reads cloud-native multidimensional arrays (Zarr V2/V3) with CF-conventions
/// metadata from a **local** store (`data_path`) or a remote **S3/HTTP** store
/// (`endpoint` + `bucket` + `path`, or an `s3://`/`http(s)://` URL in
/// `data_path`). The grid must be geographic (WGS84 lat/lon).
#[derive(Debug, Clone, Deserialize)]
pub struct ZarrConfig {
    /// Local path to the Zarr store root directory (the `.zarr` directory), or
    /// an `s3://` / `http(s)://` URL. Mutually exclusive with the S3
    /// `endpoint`/`bucket` source.
    pub data_path: Option<String>,
    /// S3-compatible endpoint URL, e.g. "https://s3.eu-central-1.amazonaws.com".
    /// Required together with `bucket`.
    pub endpoint: Option<String>,
    /// S3 bucket name. Required when `endpoint` is set.
    pub bucket: Option<String>,
    /// Path of the Zarr store within the bucket (S3 source), e.g.
    /// "zarr/2026/01/data/air_temperature_at_2_metres.zarr". Required for the
    /// remote source. For a local `data_path` it is an optional sub-path
    /// appended to the directory.
    pub path: Option<String>,
    /// Zarr metadata version to read: `2` or `3`. Default: auto-detect
    /// (try V3 metadata, fall back to V2).
    pub zarr_version: Option<u8>,
    /// Optional variable filter — only expose these variables as parameters.
    /// Default: every data variable discovered in the store.
    pub parameters: Option<Vec<String>>,
    /// Poll interval in seconds. The store is re-read on this cadence so
    /// appended time steps surface without a reload. Default: 300 (5 min).
    #[serde(default = "default_zarr_poll_interval")]
    pub poll_interval_secs: u64,
    /// Chunk LRU cache size in MB. Caches full chunk-object bytes for the
    /// byte-range reader (most useful for the remote S3/HTTP backends).
    /// Default: 256.
    #[serde(default = "default_zarr_cache_mb")]
    pub cache_mb: u64,
    /// Read the source as an **Icechunk** repository (transactional/versioned
    /// Zarr) rather than a plain Zarr store. Requires the server to be built
    /// with the `icechunk` feature. The repo location reuses `data_path`
    /// (local) or `endpoint`+`bucket`+`path` (S3); this table selects the
    /// version to read. See issue #335.
    pub icechunk: Option<IcechunkConfig>,
}

impl ZarrConfig {
    /// Defaults for an auto-discovered local Zarr store directory (#411):
    /// `data_path` is the store root, version auto-detected, every variable
    /// exposed, default poll/cache.
    pub fn auto_local(data_path: String) -> Self {
        ZarrConfig {
            data_path: Some(data_path),
            endpoint: None,
            bucket: None,
            path: None,
            zarr_version: None,
            parameters: None,
            poll_interval_secs: default_zarr_poll_interval(),
            cache_mb: default_zarr_cache_mb(),
            icechunk: None,
        }
    }
}

/// Icechunk version selector for `[collections.zarr.icechunk]`.
///
/// At most one of `branch` / `tag` / `snapshot` may be set; the default is the
/// HEAD of branch `main`.
#[derive(Debug, Clone, Deserialize)]
pub struct IcechunkConfig {
    /// Read the HEAD of this branch (default: `main`).
    pub branch: Option<String>,
    /// Read this tag.
    pub tag: Option<String>,
    /// Read this exact snapshot id (immutable).
    pub snapshot: Option<String>,
    /// S3 region for the repo's object store (e.g. "us-west-2"). Needed for
    /// AWS; ignored for the local backend.
    pub region: Option<String>,
    /// S3 path-style addressing (`endpoint/bucket/key`). Default `true` (works
    /// for S3-compatible endpoints and AWS regional endpoints); set `false` for
    /// virtual-host style. Ignored for the local backend.
    pub force_path_style: Option<bool>,
}

/// Observation table configuration for engine-postgis.
///
/// Three shapes are supported:
/// - `long` — EAV layout, one row per observation, parameter name stored in
///   `param_col`, value in `value_col`.
/// - `wide` — one row per (station, time), each parameter mapped to its own
///   column via `[[columns]]`.
/// - `per_parameter` — one table per parameter, listed under `[[tables]]`.
#[derive(Debug, Clone, Deserialize)]
pub struct PostgisConfig {
    /// Either the name of an env var holding the DSN, or — when
    /// `MC_ALLOW_INLINE_DB_URL=1` is set at config-load — a literal
    /// `postgres://` / `postgresql://` URL (dev ergonomics, WARN logged).
    pub dsn_env: String,
    /// Optional connection pool size. Hard-capped at 32 at validate time.
    pub pool_size: Option<u32>,
    /// Optional label used for the `pool_key` metrics tag when the host is
    /// ephemeral. Defaults to `<user>@<host>:<port>/<db>`.
    pub pool_label: Option<String>,
    /// Metadata cache refresh cadence. Default 300 s.
    pub metadata_refresh_secs: Option<u64>,
    /// Optional stations table. When absent (mode A), or present but with a
    /// resolvable `observations.geom_col` (mode B, orphan fallback), location
    /// coordinates are derived from the observations table's own geometry —
    /// see [`PostgisObservationsConfig::obs_geom_available`].
    #[serde(default)]
    pub stations: Option<PostgisStationsConfig>,
    pub observations: PostgisObservationsConfig,
    #[serde(default)]
    pub parameters: Vec<PostgisParameterConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PostgisStationsConfig {
    /// Qualified table name (`schema.table` or bare `table`).
    pub table: String,
    pub id_col: String,
    pub label_col: String,
    pub geom_col: String,
    #[serde(default)]
    pub property_cols: Vec<String>,
    /// Config-time-constant WHERE fragment. Not re-parsed from requests.
    #[serde(default, rename = "where")]
    pub where_clause: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PostgisObservationsConfig {
    /// One of `"long"`, `"wide"`, `"per_parameter"`.
    pub shape: String,
    /// Qualified table name. Required for `long` and `wide`, forbidden for
    /// `per_parameter` (per-parameter tables are listed under `[[tables]]`).
    pub table: Option<String>,
    /// Column joining observations to a row in `stations.table`.
    /// Required for all shapes (may be overridden per-table in `per_parameter`).
    pub station_fk_col: Option<String>,
    /// Timestamp column. Required for all shapes (inheritable in `per_parameter`).
    pub time_col: Option<String>,
    /// Mandatory when the mapped `time_col` is `timestamp without time zone`.
    /// IANA TZ name (e.g. `"UTC"`, `"Europe/Helsinki"`). Runtime type
    /// assertion via `information_schema.columns` is a TODO — see #110.
    pub time_col_tz: Option<String>,
    /// EAV parameter-name column. Only valid for `shape = "long"`.
    pub param_col: Option<String>,
    /// EAV value column. Valid for `long`; inheritable for `per_parameter`.
    pub value_col: Option<String>,
    /// Optional geometry column on the observations side (used in
    /// orphan-station handling — see plan doc amendment E).
    pub geom_col: Option<String>,
    /// Time window for deriving the location list from the observations table
    /// (only relevant when `geom_col` drives location derivation). ISO 8601
    /// duration (e.g. `"PT24H"`, `"P7D"`) — only stations that reported within
    /// this window of "now" are advertised, which keeps the `DISTINCT ON` scan
    /// on recent hypertable chunks (fast) instead of full history. Defaults to
    /// 24h when absent; set to `"all"` for full history (a climate-style
    /// collection — needs a role `statement_timeout` large enough to scan the
    /// whole table, or a pre-materialized locations table).
    pub locations_window: Option<String>,
    /// `wide` shape: one entry per parameter → column mapping.
    #[serde(default)]
    pub columns: Vec<PostgisObservationColumn>,
    /// `per_parameter` shape: one entry per parameter → table mapping.
    #[serde(default)]
    pub tables: Vec<PostgisObservationTable>,
}

impl PostgisObservationsConfig {
    /// Whether the observations side carries a resolvable geometry column for
    /// every queried table — the precondition for deriving locations from the
    /// observations table (modes A and B). For `long`/`wide` this is just
    /// `geom_col`; for `per_parameter` every `[[tables]]` entry must resolve a
    /// geom (its own `geom_col`, else the inherited `observations.geom_col`).
    pub fn obs_geom_available(&self) -> bool {
        match self.shape.as_str() {
            "per_parameter" => {
                !self.tables.is_empty()
                    && self
                        .tables
                        .iter()
                        .all(|t| t.geom_col.is_some() || self.geom_col.is_some())
            }
            // long / wide (and any not-yet-validated shape): single table.
            _ => self.geom_col.is_some(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PostgisObservationColumn {
    pub parameter: String,
    pub column: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PostgisObservationTable {
    pub parameter: String,
    pub table: String,
    pub station_fk_col: Option<String>,
    pub time_col: Option<String>,
    pub time_col_tz: Option<String>,
    pub value_col: Option<String>,
    pub geom_col: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PostgisParameterConfig {
    pub name: String,
    pub label: String,
    pub unit: String,
    pub observed_property: Option<String>,
    /// For `long`: the string literal stored in `param_col`.
    /// For `wide`/`per_parameter`: column/table key. Defaults to `name`.
    pub source_key: Option<String>,
}

/// Byte-level check for `^[A-Za-z_][A-Za-z0-9_]{0,62}$` — no regex dep.
/// Used by config validation and (re-exported) by engine-postgis identifier checks.
pub fn is_valid_sql_identifier(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes.len() > 63 {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Validate a qualified table name (`schema.table` or bare `table`). Returns
/// the (schema, table) pair with schema defaulted to `"public"`.
pub fn validate_qualified_table(name: &str) -> Result<(&str, &str), String> {
    let parts: Vec<&str> = name.split('.').collect();
    let (schema, table) = match parts.len() {
        1 => ("public", parts[0]),
        2 => (parts[0], parts[1]),
        _ => return Err(format!("invalid qualified table name '{name}'")),
    };
    if !is_valid_sql_identifier(schema) {
        return Err(format!("invalid schema identifier '{schema}' in '{name}'"));
    }
    if !is_valid_sql_identifier(table) {
        return Err(format!("invalid table identifier '{table}' in '{name}'"));
    }
    Ok((schema, table))
}

fn looks_like_db_url(s: &str) -> bool {
    s.starts_with("postgres://") || s.starts_with("postgresql://")
}

fn validate_tz_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'/' || b == b'-' || b == b'+')
}

/// Hard-fail validation for a `PostgisConfig`. Runs at load time via
/// `ServerConfig::validate()`; see issue #101 for the rule list.
fn validate_postgis(id: &str, cfg: &PostgisConfig) -> Result<(), crate::error::DataServerError> {
    use crate::error::DataServerError::Config;

    // -- DSN / inline-URL opt-in ------------------------------------------
    if cfg.dsn_env.is_empty() {
        return Err(Config(format!(
            "Collection '{id}': [postgis].dsn_env must be set (env var name, or literal URL with MC_ALLOW_INLINE_DB_URL=1)"
        )));
    }
    if looks_like_db_url(&cfg.dsn_env)
        && std::env::var("MC_ALLOW_INLINE_DB_URL").ok().as_deref() != Some("1")
    {
        return Err(Config(format!(
            "Collection '{id}': [postgis].dsn_env looks like a literal database URL — use an env var name, or set MC_ALLOW_INLINE_DB_URL=1 to opt in"
        )));
    }
    if !looks_like_db_url(&cfg.dsn_env) && !is_valid_env_var_name(&cfg.dsn_env) {
        return Err(Config(format!(
            "Collection '{id}': [postgis].dsn_env '{}' is not a valid env var name",
            cfg.dsn_env
        )));
    }

    // -- pool_size cap ----------------------------------------------------
    if let Some(n) = cfg.pool_size {
        if n == 0 {
            return Err(Config(format!(
                "Collection '{id}': [postgis].pool_size must be > 0"
            )));
        }
        if n > 32 {
            return Err(Config(format!(
                "Collection '{id}': [postgis].pool_size {n} exceeds hard cap 32"
            )));
        }
    }

    // -- parameters list --------------------------------------------------
    if cfg.parameters.is_empty() {
        return Err(Config(format!(
            "Collection '{id}': [postgis] requires at least one [[parameters]] entry"
        )));
    }
    let mut param_names = std::collections::HashSet::new();
    for p in &cfg.parameters {
        if p.name.is_empty() {
            return Err(Config(format!(
                "Collection '{id}': parameter has an empty 'name' field"
            )));
        }
        if !param_names.insert(p.name.as_str()) {
            return Err(Config(format!(
                "Collection '{id}': duplicate parameter name '{}'",
                p.name
            )));
        }
        if p.label.is_empty() {
            return Err(Config(format!(
                "Collection '{id}': parameter '{}' has empty 'label'",
                p.name
            )));
        }
        if p.unit.is_empty() {
            return Err(Config(format!(
                "Collection '{id}': parameter '{}' has empty 'unit'",
                p.name
            )));
        }
    }

    // -- stations (optional) ----------------------------------------------
    // The stations table may be omitted entirely (mode A): locations are then
    // derived from the observations table's own geometry. When present, all
    // identifiers are validated exactly as before.
    if let Some(s) = &cfg.stations {
        validate_qualified_table(&s.table)
            .map_err(|e| Config(format!("Collection '{id}': stations.table: {e}")))?;
        for (field, value) in [
            ("id_col", &s.id_col),
            ("label_col", &s.label_col),
            ("geom_col", &s.geom_col),
        ] {
            if !is_valid_sql_identifier(value) {
                return Err(Config(format!(
                    "Collection '{id}': stations.{field} '{value}' is not a valid SQL identifier"
                )));
            }
        }
        for (i, col) in s.property_cols.iter().enumerate() {
            if !is_valid_sql_identifier(col) {
                return Err(Config(format!(
                    "Collection '{id}': stations.property_cols[{i}] '{col}' is not a valid SQL identifier"
                )));
            }
        }
        if let Some(w) = s.where_clause.as_deref() {
            validate_stations_where_clause(id, w)?;
        }
    } else if !cfg.observations.obs_geom_available() {
        // No stations table AND no resolvable observations geometry — there is
        // no way to place any location. Reject with an actionable message.
        return Err(Config(format!(
            "Collection '{id}': no [postgis.stations] table and no resolvable observations geometry — set observations.geom_col (per_parameter: on every table or as the shared default) or add a [postgis.stations] table"
        )));
    }

    // -- observations (shape-dependent) -----------------------------------
    let o = &cfg.observations;
    match o.shape.as_str() {
        "long" => validate_observations_long(id, o)?,
        "wide" => validate_observations_wide(id, o)?,
        "per_parameter" => validate_observations_per_parameter(id, o)?,
        other => {
            return Err(Config(format!(
                "Collection '{id}': observations.shape '{other}' is not one of 'long', 'wide', 'per_parameter'"
            )));
        }
    }

    // -- observations.locations_window (optional) -------------------------
    // ISO 8601 duration, or the sentinel "all" (full history). Absent ⇒ 24h
    // default (applied at engine resolve). Validate the string parses now so a
    // typo fails at config load, not at the first metadata refresh.
    if let Some(w) = o.locations_window.as_deref() {
        if !w.eq_ignore_ascii_case("all") {
            crate::datetime::parse_iso8601_duration(w).map_err(|e| {
                Config(format!(
                    "Collection '{id}': observations.locations_window '{w}' is not a valid ISO 8601 duration (or \"all\"): {e}"
                ))
            })?;
        }
    }

    // -- parameters cross-ref --------------------------------------------
    match o.shape.as_str() {
        "wide" => {
            let cols: std::collections::HashSet<&str> =
                o.columns.iter().map(|c| c.parameter.as_str()).collect();
            for p in &cfg.parameters {
                let key = p.source_key.as_deref().unwrap_or(p.name.as_str());
                if !cols.contains(key) {
                    return Err(Config(format!(
                        "Collection '{id}': parameter '{}' (source_key '{key}') is not mapped in observations.columns",
                        p.name
                    )));
                }
            }
        }
        "per_parameter" => {
            let tables: std::collections::HashSet<&str> =
                o.tables.iter().map(|t| t.parameter.as_str()).collect();
            for p in &cfg.parameters {
                let key = p.source_key.as_deref().unwrap_or(p.name.as_str());
                if !tables.contains(key) {
                    return Err(Config(format!(
                        "Collection '{id}': parameter '{}' (source_key '{key}') is not mapped in observations.tables",
                        p.name
                    )));
                }
            }
        }
        _ => {} // long: source_key is a string literal, no cross-ref
    }

    Ok(())
}

/// Reject obviously-dangerous `stations.where_clause` contents before the
/// engine inlines the string into SQL. Not a cryptographic guarantee —
/// an operator with config-file write access can always break things —
/// but blocks the common footgun of a typo (semicolons, comments, DML
/// verbs) turning into a destructive query. Anything richer than a
/// simple `col = value AND col < value` filter should live in a SQL
/// `VIEW`, which is the documented exit strategy.
fn validate_stations_where_clause(id: &str, w: &str) -> Result<(), crate::error::DataServerError> {
    use crate::error::DataServerError::Config;

    const MAX_LEN: usize = 512;
    if w.len() > MAX_LEN {
        return Err(Config(format!(
            "Collection '{id}': stations.where_clause exceeds {MAX_LEN}-byte cap"
        )));
    }
    if w.contains(';') {
        return Err(Config(format!(
            "Collection '{id}': stations.where_clause must not contain ';' (statement chaining)"
        )));
    }
    if w.contains("--") || w.contains("/*") || w.contains("*/") {
        return Err(Config(format!(
            "Collection '{id}': stations.where_clause must not contain SQL comment markers"
        )));
    }
    // Whole-word check for write/DDL verbs. Case-insensitive; we collapse
    // all whitespace to single spaces first so that tab/newline-separated
    // verbs (`active\nunion\nselect 1`) are caught the same as space-
    // separated ones. Postgres treats any whitespace as a token separator,
    // so the check must match.
    let lower = w.to_ascii_lowercase();
    let normalized = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    let padded = format!(" {normalized} ");
    for verb in [
        // DML / DDL
        "drop", "delete", "update", "insert", "truncate", "alter", "create", "grant", "revoke",
        "copy",
        // Data-exfil / dynamic-execution vectors flagged in PR review:
        // UNION SELECT is the common SQLi exfil pattern; EXECUTE runs
        // dynamic SQL; CALL / PERFORM invoke stored functions; SELECT /
        // FROM catch correlated subqueries like
        // `territory = (SELECT x FROM y)`.
        "union", "execute", "call", "perform", "select", "from",
    ] {
        let needle = format!(" {verb} ");
        if padded.contains(&needle) {
            return Err(Config(format!(
                "Collection '{id}': stations.where_clause must not contain '{verb}' — put filter logic in a SQL VIEW instead"
            )));
        }
    }
    Ok(())
}

fn is_valid_env_var_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().enumerate().all(|(i, b)| match (i, b) {
            (0, b'0'..=b'9') => false,
            (_, b'A'..=b'Z') | (_, b'a'..=b'z') | (_, b'0'..=b'9') | (_, b'_') => true,
            _ => false,
        })
}

fn validate_observations_long(
    id: &str,
    o: &PostgisObservationsConfig,
) -> Result<(), crate::error::DataServerError> {
    use crate::error::DataServerError::Config;

    let table = o.table.as_deref().ok_or_else(|| {
        Config(format!(
            "Collection '{id}': observations.shape = 'long' requires observations.table"
        ))
    })?;
    validate_qualified_table(table)
        .map_err(|e| Config(format!("Collection '{id}': observations.table: {e}")))?;

    let required = [
        ("station_fk_col", &o.station_fk_col),
        ("time_col", &o.time_col),
        ("param_col", &o.param_col),
        ("value_col", &o.value_col),
    ];
    for (field, value) in required {
        let v = value.as_deref().ok_or_else(|| {
            Config(format!(
                "Collection '{id}': observations.shape = 'long' requires observations.{field}"
            ))
        })?;
        if !is_valid_sql_identifier(v) {
            return Err(Config(format!(
                "Collection '{id}': observations.{field} '{v}' is not a valid SQL identifier"
            )));
        }
    }

    if !o.columns.is_empty() {
        return Err(Config(format!(
            "Collection '{id}': observations.shape = 'long' does not allow [[observations.columns]]"
        )));
    }
    if !o.tables.is_empty() {
        return Err(Config(format!(
            "Collection '{id}': observations.shape = 'long' does not allow [[observations.tables]]"
        )));
    }

    if let Some(tz) = &o.time_col_tz {
        if !validate_tz_name(tz) {
            return Err(Config(format!(
                "Collection '{id}': observations.time_col_tz '{tz}' is not a valid IANA-like name"
            )));
        }
    }
    if let Some(g) = &o.geom_col {
        if !is_valid_sql_identifier(g) {
            return Err(Config(format!(
                "Collection '{id}': observations.geom_col '{g}' is not a valid SQL identifier"
            )));
        }
    }
    Ok(())
}

fn validate_observations_wide(
    id: &str,
    o: &PostgisObservationsConfig,
) -> Result<(), crate::error::DataServerError> {
    use crate::error::DataServerError::Config;

    let table = o.table.as_deref().ok_or_else(|| {
        Config(format!(
            "Collection '{id}': observations.shape = 'wide' requires observations.table"
        ))
    })?;
    validate_qualified_table(table)
        .map_err(|e| Config(format!("Collection '{id}': observations.table: {e}")))?;

    for (field, value) in [
        ("station_fk_col", &o.station_fk_col),
        ("time_col", &o.time_col),
    ] {
        let v = value.as_deref().ok_or_else(|| {
            Config(format!(
                "Collection '{id}': observations.shape = 'wide' requires observations.{field}"
            ))
        })?;
        if !is_valid_sql_identifier(v) {
            return Err(Config(format!(
                "Collection '{id}': observations.{field} '{v}' is not a valid SQL identifier"
            )));
        }
    }

    if o.param_col.is_some() {
        return Err(Config(format!(
            "Collection '{id}': observations.shape = 'wide' does not allow observations.param_col"
        )));
    }
    if o.value_col.is_some() {
        return Err(Config(format!(
            "Collection '{id}': observations.shape = 'wide' does not allow observations.value_col (use [[observations.columns]])"
        )));
    }
    if !o.tables.is_empty() {
        return Err(Config(format!(
            "Collection '{id}': observations.shape = 'wide' does not allow [[observations.tables]]"
        )));
    }
    if o.columns.is_empty() {
        return Err(Config(format!(
            "Collection '{id}': observations.shape = 'wide' requires at least one [[observations.columns]] entry"
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for (i, c) in o.columns.iter().enumerate() {
        if c.parameter.is_empty() {
            return Err(Config(format!(
                "Collection '{id}': observations.columns[{i}] has empty 'parameter'"
            )));
        }
        if !seen.insert(c.parameter.as_str()) {
            return Err(Config(format!(
                "Collection '{id}': observations.columns has duplicate parameter '{}'",
                c.parameter
            )));
        }
        if !is_valid_sql_identifier(&c.column) {
            return Err(Config(format!(
                "Collection '{id}': observations.columns[{i}].column '{}' is not a valid SQL identifier",
                c.column
            )));
        }
    }
    if let Some(tz) = &o.time_col_tz {
        if !validate_tz_name(tz) {
            return Err(Config(format!(
                "Collection '{id}': observations.time_col_tz '{tz}' is not a valid IANA-like name"
            )));
        }
    }
    if let Some(g) = &o.geom_col {
        if !is_valid_sql_identifier(g) {
            return Err(Config(format!(
                "Collection '{id}': observations.geom_col '{g}' is not a valid SQL identifier"
            )));
        }
    }
    Ok(())
}

fn validate_observations_per_parameter(
    id: &str,
    o: &PostgisObservationsConfig,
) -> Result<(), crate::error::DataServerError> {
    use crate::error::DataServerError::Config;

    if o.table.is_some() {
        return Err(Config(format!(
            "Collection '{id}': observations.shape = 'per_parameter' does not allow observations.table (tables go under [[observations.tables]])"
        )));
    }
    if o.param_col.is_some() {
        return Err(Config(format!(
            "Collection '{id}': observations.shape = 'per_parameter' does not allow observations.param_col"
        )));
    }
    if !o.columns.is_empty() {
        return Err(Config(format!(
            "Collection '{id}': observations.shape = 'per_parameter' does not allow [[observations.columns]]"
        )));
    }
    if o.tables.is_empty() {
        return Err(Config(format!(
            "Collection '{id}': observations.shape = 'per_parameter' requires at least one [[observations.tables]] entry"
        )));
    }

    // Observations-level defaults (optional, inheritable per-table).
    for (field, value) in [
        ("station_fk_col", &o.station_fk_col),
        ("time_col", &o.time_col),
        ("value_col", &o.value_col),
        ("geom_col", &o.geom_col),
    ] {
        if let Some(v) = value.as_deref() {
            if !is_valid_sql_identifier(v) {
                return Err(Config(format!(
                    "Collection '{id}': observations.{field} '{v}' is not a valid SQL identifier"
                )));
            }
        }
    }
    if let Some(tz) = &o.time_col_tz {
        if !validate_tz_name(tz) {
            return Err(Config(format!(
                "Collection '{id}': observations.time_col_tz '{tz}' is not a valid IANA-like name"
            )));
        }
    }

    let mut seen_params = std::collections::HashSet::new();
    for (i, t) in o.tables.iter().enumerate() {
        if t.parameter.is_empty() {
            return Err(Config(format!(
                "Collection '{id}': observations.tables[{i}] has empty 'parameter'"
            )));
        }
        if !seen_params.insert(t.parameter.as_str()) {
            return Err(Config(format!(
                "Collection '{id}': observations.tables has duplicate parameter '{}'",
                t.parameter
            )));
        }
        validate_qualified_table(&t.table).map_err(|e| {
            Config(format!(
                "Collection '{id}': observations.tables[{i}].table: {e}"
            ))
        })?;

        // Resolve each column: per-table override wins, otherwise inherit.
        let resolved = [
            (
                "station_fk_col",
                t.station_fk_col.as_deref().or(o.station_fk_col.as_deref()),
            ),
            ("time_col", t.time_col.as_deref().or(o.time_col.as_deref())),
            (
                "value_col",
                t.value_col.as_deref().or(o.value_col.as_deref()),
            ),
        ];
        for (field, value) in resolved {
            let v = value.ok_or_else(|| {
                Config(format!(
                    "Collection '{id}': observations.tables[{i}] ('{}') missing '{field}' (no per-table override and no observations-level default)",
                    t.parameter
                ))
            })?;
            if !is_valid_sql_identifier(v) {
                return Err(Config(format!(
                    "Collection '{id}': observations.tables[{i}].{field} '{v}' is not a valid SQL identifier"
                )));
            }
        }

        if let Some(tz) = &t.time_col_tz {
            if !validate_tz_name(tz) {
                return Err(Config(format!(
                    "Collection '{id}': observations.tables[{i}].time_col_tz '{tz}' is not a valid IANA-like name"
                )));
            }
        }
        if let Some(g) = &t.geom_col {
            if !is_valid_sql_identifier(g) {
                return Err(Config(format!(
                    "Collection '{id}': observations.tables[{i}].geom_col '{g}' is not a valid SQL identifier"
                )));
            }
        }
    }
    Ok(())
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

            let raw: toml::Table = toml::from_str(&content).map_err(|e| {
                crate::error::DataServerError::Config(format!("Failed to parse {filename}: {e}"))
            })?;
            // style_bundles must live in config.toml; serde silently drops
            // them on CollectionConfig, which makes the later "not defined"
            // error confusing.
            if raw.contains_key("style_bundles") {
                return Err(crate::error::DataServerError::Config(format!(
                    "{filename}: [[style_bundles]] is not allowed in per-collection files — \
                     move the block to the top-level config.toml"
                )));
            }
            let collection: CollectionConfig = raw.try_into().map_err(|e| {
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
            // Extras must have non-empty, non-"default", unique names.
            // "default" is reserved for the bundle's default style; an extra
            // using that name would silently overwrite it in
            // admin::build_styles. Duplicate names within the same bundle
            // would silently overwrite each other in the same HashMap.
            let mut extra_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
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
                if !extra_names.insert(extra.name.as_str()) {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Style bundle '{}': duplicate extra name '{}'",
                        bundle.id, extra.name
                    )));
                }
                if extra.parameter.as_deref() == Some("") {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Style bundle '{}': extra '{}' has an empty 'parameter' field",
                        bundle.id, extra.name
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

            // Keywords must be non-empty (empty entries produce blank
            // `<Keyword/>` elements / dead `?q=` facets). Entries are already
            // trimmed by `de_trimmed_keywords` at load, so an all-whitespace
            // keyword arrives here as "" — a plain `is_empty()` check suffices.
            if collection.keywords.iter().any(|k| k.is_empty()) {
                return Err(crate::error::DataServerError::Config(format!(
                    "Collection '{id}': 'keywords' must not contain empty strings"
                )));
            }

            // License, when present, needs a non-empty title and an http(s) url
            // (the synthesized SPDX url is always http(s), so only an explicit
            // url can be malformed here).
            if let Some(license) = &collection.license {
                // Title is trimmed at load by `de_trimmed_string`, so an
                // all-whitespace title arrives here as "" — `is_empty()` suffices.
                if license.title.is_empty() {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Collection '{id}': [collections.license] 'title' must not be empty"
                    )));
                }
                if let Some(url) = &license.url {
                    // `url` is trimmed at load by `de_trimmed_opt_string`.
                    if !(url.starts_with("http://") || url.starts_with("https://")) {
                        return Err(crate::error::DataServerError::Config(format!(
                            "Collection '{id}': [collections.license] 'url' must be an http(s) URL"
                        )));
                    }
                }
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

                // Data source: exactly one of local `data_path` or S3
                // `endpoint`+`bucket`. The remote source needs *both* S3 fields
                // plus `prefix_pattern` (date/run templating); local needs none
                // of those.
                let has_local = grib.data_path.is_some();
                let has_any_remote = grib.endpoint.is_some() || grib.bucket.is_some();
                if has_local && has_any_remote {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Collection '{id}': grib 'data_path' (local) is mutually exclusive \
                         with 'endpoint'/'bucket' (S3)"
                    )));
                }
                if !has_local && !has_any_remote {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Collection '{id}': grib requires either 'data_path' (local) or \
                         'endpoint'+'bucket' (S3)"
                    )));
                }
                if has_any_remote {
                    // Partial remote config (one of endpoint/bucket) is invalid.
                    if grib.endpoint.is_none() || grib.bucket.is_none() {
                        return Err(crate::error::DataServerError::Config(format!(
                            "Collection '{id}': remote grib requires both 'endpoint' and 'bucket'"
                        )));
                    }
                    if grib.prefix_pattern.is_none() {
                        return Err(crate::error::DataServerError::Config(format!(
                            "Collection '{id}': remote grib (endpoint+bucket) requires 'prefix_pattern'"
                        )));
                    }
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

            // Zarr engine requires zarr config section.
            if collection.engine_type == "zarr" && collection.zarr.is_none() {
                return Err(crate::error::DataServerError::Config(format!(
                    "Collection '{id}': engine_type 'zarr' requires a [collections.zarr] config section"
                )));
            }
            if collection.zarr.is_some() && collection.engine_type != "zarr" {
                return Err(crate::error::DataServerError::Config(format!(
                    "Collection '{id}': [collections.zarr] is set but engine_type is '{}'",
                    collection.engine_type
                )));
            }
            if let Some(zarr) = &collection.zarr {
                if zarr.poll_interval_secs == 0 {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Collection '{id}': zarr poll_interval_secs must be > 0"
                    )));
                }

                // Data source: exactly one of local `data_path` or S3
                // `endpoint`+`bucket`. The remote source needs both S3 fields
                // plus `path` (the store location within the bucket).
                let has_local = zarr.data_path.is_some();
                let has_any_remote = zarr.endpoint.is_some() || zarr.bucket.is_some();
                if has_local && has_any_remote {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Collection '{id}': zarr 'data_path' (local) is mutually exclusive \
                         with 'endpoint'/'bucket' (S3)"
                    )));
                }
                if !has_local && !has_any_remote {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Collection '{id}': zarr requires either 'data_path' (local) or \
                         'endpoint'+'bucket' (S3)"
                    )));
                }
                if has_any_remote {
                    if zarr.endpoint.is_none() || zarr.bucket.is_none() {
                        return Err(crate::error::DataServerError::Config(format!(
                            "Collection '{id}': remote zarr requires both 'endpoint' and 'bucket'"
                        )));
                    }
                    if zarr.path.is_none() {
                        return Err(crate::error::DataServerError::Config(format!(
                            "Collection '{id}': remote zarr (endpoint+bucket) requires 'path'"
                        )));
                    }
                }

                if let Some(v) = zarr.zarr_version {
                    if v != 2 && v != 3 {
                        return Err(crate::error::DataServerError::Config(format!(
                            "Collection '{id}': invalid zarr_version {v}, expected 2 or 3"
                        )));
                    }
                }

                // `path` is joined onto the source root; an absolute path or one
                // with `..` would silently escape it (`PathBuf::join` discards
                // the base on an absolute child), so reject those at load.
                if let Some(p) = &zarr.path {
                    if std::path::Path::new(p).is_absolute()
                        || p.split(['/', '\\']).any(|c| c == "..")
                    {
                        return Err(crate::error::DataServerError::Config(format!(
                            "Collection '{id}': zarr 'path' must be a relative path without \
                             '..' components"
                        )));
                    }
                }

                // Icechunk: at most one version selector (branch/tag/snapshot).
                if let Some(ic) = &zarr.icechunk {
                    let selectors = [ic.branch.is_some(), ic.tag.is_some(), ic.snapshot.is_some()]
                        .iter()
                        .filter(|&&s| s)
                        .count();
                    if selectors > 1 {
                        return Err(crate::error::DataServerError::Config(format!(
                            "Collection '{id}': zarr icechunk takes at most one of \
                             'branch'/'tag'/'snapshot'"
                        )));
                    }
                    // An empty selector would pass the count check but fail
                    // opaquely at runtime — reject it at load.
                    if [&ic.branch, &ic.tag, &ic.snapshot]
                        .iter()
                        .any(|s| s.as_deref().is_some_and(str::is_empty))
                    {
                        return Err(crate::error::DataServerError::Config(format!(
                            "Collection '{id}': zarr icechunk 'branch'/'tag'/'snapshot' must not \
                             be empty"
                        )));
                    }
                }
            }

            // PostGIS engine: requires [postgis] section + parameters + valid shape.
            if collection.engine_type == "postgis" {
                let postgis = collection.postgis.as_ref().ok_or_else(|| {
                    crate::error::DataServerError::Config(format!(
                        "Collection '{id}': engine_type 'postgis' requires a [collections.postgis] config section"
                    ))
                })?;
                validate_postgis(id, postgis)?;
            } else if collection.postgis.is_some() {
                return Err(crate::error::DataServerError::Config(format!(
                    "Collection '{id}': [collections.postgis] is set but engine_type is '{}'",
                    collection.engine_type
                )));
            }

            // CAP engine: requires [cap] section; source is data_path XOR feed_url.
            if collection.engine_type == "cap" && collection.cap.is_none() {
                return Err(crate::error::DataServerError::Config(format!(
                    "Collection '{id}': engine_type 'cap' requires a [collections.cap] config section"
                )));
            }
            if collection.cap.is_some() && collection.engine_type != "cap" {
                return Err(crate::error::DataServerError::Config(format!(
                    "Collection '{id}': [collections.cap] is set but engine_type is '{}'",
                    collection.engine_type
                )));
            }
            if let Some(cap) = &collection.cap {
                if cap.poll_interval_secs == 0 {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Collection '{id}': cap poll_interval_secs must be > 0"
                    )));
                }

                // Data source: exactly one of local `data_path` or `feed_url`
                // (mirrors the geotiff/grib/zarr local-vs-remote mutual exclusion).
                let has_local = cap.data_path.is_some();
                let has_feed = cap.feed_url.is_some();
                if has_local && has_feed {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Collection '{id}': cap 'data_path' (local) is mutually exclusive with \
                         'feed_url'"
                    )));
                }
                if !has_local && !has_feed {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Collection '{id}': cap requires either 'data_path' (local) or 'feed_url'"
                    )));
                }
                if let Some(url) = &cap.feed_url {
                    if !(url.starts_with("http://") || url.starts_with("https://")) {
                        return Err(crate::error::DataServerError::Config(format!(
                            "Collection '{id}': cap 'feed_url' must be an http(s) URL"
                        )));
                    }
                }

                // feed_allowlist entries (SSRF prefixes) must be http(s) and are
                // meaningless without a feed source.
                if !cap.feed_allowlist.is_empty() && cap.feed_url.is_none() {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Collection '{id}': cap 'feed_allowlist' requires 'feed_url'"
                    )));
                }
                for prefix in &cap.feed_allowlist {
                    if !(prefix.starts_with("http://") || prefix.starts_with("https://")) {
                        return Err(crate::error::DataServerError::Config(format!(
                            "Collection '{id}': cap 'feed_allowlist' entries must be http(s) URL \
                             prefixes"
                        )));
                    }
                    // Require a trailing '/' so a prefix can't widen unexpectedly:
                    // `https://cdn/cap` would also admit `https://cdn/cap-staging/…`,
                    // letting a feed reach adjacent paths on a trusted host.
                    if !prefix.ends_with('/') {
                        return Err(crate::error::DataServerError::Config(format!(
                            "Collection '{id}': cap 'feed_allowlist' entry '{prefix}' must end \
                             with '/' (a path prefix) to avoid unintended prefix widening"
                        )));
                    }
                }

                // `language`, when present, must be a non-empty tag (trimmed at
                // load, so an all-whitespace value arrives here as "").
                if cap.language.as_deref() == Some("") {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Collection '{id}': cap 'language' must not be empty"
                    )));
                }

                // Each status filter entry must be non-empty (a blank entry can
                // never match a CAP `<status>` and is almost certainly a typo).
                if cap.status_filter.iter().any(|s| s.trim().is_empty()) {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Collection '{id}': cap 'status_filter' must not contain empty strings"
                    )));
                }

                // `default_ttl`, when present, must be a positive ISO 8601 duration.
                if let Some(ttl) = &cap.default_ttl {
                    crate::datetime::parse_iso8601_duration(ttl).map_err(|e| {
                        crate::error::DataServerError::Config(format!(
                            "Collection '{id}': cap 'default_ttl' is not a valid positive ISO 8601 \
                             duration: {e}"
                        ))
                    })?;
                }

                // A circle needs at least a triangle to have area.
                if cap.circle_segments < 3 {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Collection '{id}': cap 'circle_segments' must be >= 3"
                    )));
                }

                // The geocode lookup property must be non-empty (it names the
                // GeoJSON property holding the zone code).
                if cap.geocode_geometry.is_some() && cap.geocode_property.trim().is_empty() {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Collection '{id}': cap 'geocode_property' must not be empty"
                    )));
                }
                if cap.geocode_value_name.as_deref() == Some("") {
                    return Err(crate::error::DataServerError::Config(format!(
                        "Collection '{id}': cap 'geocode_value_name' must not be empty"
                    )));
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

    #[test]
    fn default_for_auto_is_loopback_and_empty() {
        let cfg = ServerConfig::default_for_auto();
        assert_eq!(cfg.server.host, "127.0.0.1");
        assert_eq!(cfg.server.port, 8000);
        assert!(cfg.server.base_url.is_none());
        assert!(!cfg.server.watch_collections_dir);
        assert!(cfg.collections.is_empty());
        assert!(cfg.style_bundles.is_empty());
        // No BASE_URL env in the test process => derived from host:port.
        // (Guard against a stray env var in the harness.)
        if std::env::var("BASE_URL").is_err() {
            assert_eq!(cfg.server.base_url(), "http://127.0.0.1:8000");
        }
    }

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
    fn keywords_and_license_parse_from_toml() {
        let toml = r#"
id = "radar"
title = "Radar"
description = "d"
keywords = ["radar", "  precipitation  ", "Finland", "radar"]

[license]
title = "  CC-BY 4.0  "
url = "https://creativecommons.org/licenses/by/4.0/"
"#;
        let c: CollectionConfig = toml::from_str(toml).unwrap();
        // Keywords are trimmed and de-duplicated (the second "radar" is dropped);
        // the license title is trimmed too.
        assert_eq!(c.keywords, ["radar", "precipitation", "Finland"]);
        let lic = c.license.expect("license present");
        assert_eq!(lic.title, "CC-BY 4.0");
        assert_eq!(
            lic.resolved_url().as_deref(),
            Some("https://creativecommons.org/licenses/by/4.0/")
        );
    }

    #[test]
    fn keywords_and_license_default_absent() {
        let c: CollectionConfig =
            toml::from_str("id=\"x\"\ntitle=\"t\"\ndescription=\"d\"\n").unwrap();
        assert!(c.keywords.is_empty());
        assert!(c.license.is_none());
    }

    #[test]
    fn license_url_synthesized_from_spdx_id() {
        let lic = LicenseConfig {
            title: "Apache-2.0".into(),
            url: None,
        };
        assert_eq!(
            lic.resolved_url().as_deref(),
            Some("https://spdx.org/licenses/Apache-2.0.html")
        );
        assert_eq!(
            lic.card_link(),
            Some((
                "Apache-2.0".to_string(),
                "https://spdx.org/licenses/Apache-2.0.html".to_string()
            ))
        );
    }

    #[test]
    fn license_url_is_trimmed() {
        // A stray space in the TOML value must not leak into the stored field
        // or the emitted href — trimming happens at load (de_trimmed_opt_string).
        let lic: LicenseConfig =
            toml::from_str("title = \"X\"\nurl = \"  https://example.com/lic  \"").unwrap();
        assert_eq!(lic.url.as_deref(), Some("https://example.com/lic"));
        assert_eq!(
            lic.resolved_url().as_deref(),
            Some("https://example.com/lic")
        );
    }

    #[test]
    fn license_freetext_without_url_has_no_link_but_keeps_label() {
        // A free-text name (spaces) is not a plausible SPDX id, so no URL is
        // synthesized: the JSON `rel="license"` link is omitted (card_link →
        // None), but the display label keeps the name (card_label → (name, None))
        // so HTML/WMS can still show it.
        let lic = LicenseConfig {
            title: "All rights reserved".into(),
            url: None,
        };
        assert_eq!(lic.resolved_url(), None);
        assert_eq!(lic.card_link(), None);
        assert_eq!(lic.card_label(), ("All rights reserved".to_string(), None));
    }

    fn collection_with(extra: &str) -> ServerConfig {
        let toml = format!(
            "[server]\nhost=\"127.0.0.1\"\nport=8000\n\n\
             [[collections]]\nid=\"c\"\ntitle=\"t\"\ndescription=\"d\"\n{extra}"
        );
        toml::from_str(&toml).unwrap()
    }

    fn grib_collection(grib_body: &str) -> ServerConfig {
        collection_with(&format!(
            "engine_type = \"grib\"\n[collections.grib]\n{grib_body}"
        ))
    }

    fn cap_collection(cap_body: &str) -> ServerConfig {
        collection_with(&format!(
            "engine_type = \"cap\"\napis = [\"features\", \"wms\"]\n\
             [collections.cap]\n{cap_body}"
        ))
    }

    #[test]
    fn cap_local_data_path_validates() {
        let cfg = cap_collection("data_path = \"testdata/cap\"\n");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn cap_feed_url_validates() {
        let cfg = cap_collection("feed_url = \"https://example.org/feed.atom\"\n");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn cap_rejects_both_sources() {
        let cfg = cap_collection("data_path = \"x\"\nfeed_url = \"https://e/f\"\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn cap_rejects_no_source() {
        let cfg = cap_collection("language = \"en\"\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn cap_rejects_non_http_feed() {
        let cfg = cap_collection("feed_url = \"ftp://e/f\"\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn cap_feed_allowlist_validates_and_requires_feed() {
        // Valid http(s) prefixes alongside a feed.
        let ok = cap_collection(
            "feed_url = \"https://e/f\"\nfeed_allowlist = [\"https://cdn.example/cap/\"]\n",
        );
        assert!(ok.validate().is_ok());
        // Non-http prefix rejected.
        let bad = cap_collection("feed_url = \"https://e/f\"\nfeed_allowlist = [\"ftp://x/\"]\n");
        assert!(bad.validate().is_err());
        // Missing trailing slash rejected (prefix-widening guard).
        let no_slash = cap_collection(
            "feed_url = \"https://e/f\"\nfeed_allowlist = [\"https://cdn.example/cap\"]\n",
        );
        assert!(no_slash.validate().is_err());
        // Allowlist without a feed is meaningless → rejected.
        let no_feed = cap_collection("data_path = \"x\"\nfeed_allowlist = [\"https://cdn/\"]\n");
        assert!(no_feed.validate().is_err());
    }

    #[test]
    fn cap_rejects_empty_language() {
        let cfg = cap_collection("data_path = \"x\"\nlanguage = \"  \"\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn cap_rejects_bad_ttl_and_small_circle() {
        assert!(
            cap_collection("data_path = \"x\"\ndefault_ttl = \"banana\"\n")
                .validate()
                .is_err()
        );
        assert!(cap_collection("data_path = \"x\"\ncircle_segments = 2\n")
            .validate()
            .is_err());
    }

    #[test]
    fn cap_section_requires_cap_engine_type() {
        // [collections.cap] present but engine_type defaults to csv → rejected.
        let cfg = collection_with("[collections.cap]\ndata_path = \"x\"\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn cap_engine_requires_cap_section() {
        let cfg = collection_with("engine_type = \"cap\"\napis = [\"features\"]\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn cap_defaults_parse() {
        let cfg = cap_collection("data_path = \"testdata/cap\"\n");
        let cap = cfg.collections[0].cap.as_ref().unwrap();
        assert_eq!(cap.poll_interval_secs, 300);
        assert_eq!(cap.circle_segments, 64);
        assert_eq!(cap.status_filter, vec!["Actual".to_string()]);
        assert!(cap.language.is_none());
        assert_eq!(cap.geocode_property, "code");
        assert!(cap.geocode_geometry.is_none());
    }

    #[test]
    fn cap_geocode_fields_validate() {
        let ok = cap_collection(
            "data_path = \"x\"\ngeocode_geometry = \"emma.geojson\"\n\
             geocode_property = \"code\"\ngeocode_value_name = \"EMMA_ID\"\n",
        );
        assert!(ok.validate().is_ok());
        // Empty property / value_name rejected.
        assert!(cap_collection(
            "data_path = \"x\"\ngeocode_geometry = \"e.geojson\"\ngeocode_property = \"\"\n"
        )
        .validate()
        .is_err());
        assert!(
            cap_collection("data_path = \"x\"\ngeocode_value_name = \"\"\n")
                .validate()
                .is_err()
        );
    }

    #[test]
    fn grib_local_data_path_validates() {
        // Local source: data_path alone, no prefix_pattern needed.
        let cfg = grib_collection("data_path = \"testdata/grib-local\"\n");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn grib_remote_validates_with_prefix_pattern() {
        let cfg = grib_collection(
            "endpoint = \"https://s3\"\nbucket = \"b\"\nprefix_pattern = \"%Y%m%d/\"\n",
        );
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn grib_rejects_data_path_with_s3() {
        // Mutual exclusivity: local + S3 in one config is an error.
        let cfg = grib_collection("data_path = \"x\"\nendpoint = \"https://s3\"\nbucket = \"b\"\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn grib_rejects_no_data_source() {
        let cfg = grib_collection("index_format = \"ecmwf-json\"\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn grib_remote_rejects_missing_prefix_pattern() {
        let cfg = grib_collection("endpoint = \"https://s3\"\nbucket = \"b\"\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn grib_rejects_partial_remote() {
        // endpoint without bucket (and vice versa) must be rejected at load,
        // not silently slip past validate() to fail later in the engine.
        let only_endpoint =
            grib_collection("endpoint = \"https://s3\"\nprefix_pattern = \"%Y/\"\n");
        assert!(only_endpoint.validate().is_err());
        let only_bucket = grib_collection("bucket = \"b\"\nprefix_pattern = \"%Y/\"\n");
        assert!(only_bucket.validate().is_err());
    }

    fn zarr_collection(zarr_body: &str) -> ServerConfig {
        collection_with(&format!(
            "engine_type = \"zarr\"\n[collections.zarr]\n{zarr_body}"
        ))
    }

    #[test]
    fn zarr_local_data_path_validates() {
        let cfg = zarr_collection("data_path = \"testdata/zarr-era5-t2m\"\nzarr_version = 3\n");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn zarr_remote_validates_with_path() {
        let cfg =
            zarr_collection("endpoint = \"https://s3\"\nbucket = \"b\"\npath = \"data.zarr\"\n");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn zarr_engine_requires_section() {
        // engine_type = "zarr" with no [collections.zarr] section is rejected.
        let cfg = collection_with("engine_type = \"zarr\"\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn zarr_section_requires_matching_engine_type() {
        let cfg = collection_with("[collections.zarr]\ndata_path = \"x\"\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn zarr_rejects_data_path_with_s3() {
        let cfg = zarr_collection("data_path = \"x\"\nendpoint = \"https://s3\"\nbucket = \"b\"\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn zarr_rejects_no_data_source() {
        let cfg = zarr_collection("zarr_version = 3\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn zarr_remote_rejects_missing_path() {
        let cfg = zarr_collection("endpoint = \"https://s3\"\nbucket = \"b\"\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn zarr_rejects_invalid_version() {
        let cfg = zarr_collection("data_path = \"x\"\nzarr_version = 4\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn zarr_icechunk_single_version_selector_ok() {
        let cfg = zarr_collection(
            "data_path = \"x\"\n[collections.zarr.icechunk]\nsnapshot = \"abc123\"\n",
        );
        assert!(cfg.validate().is_ok());
        // Default (no selector) is also fine — implies branch main HEAD.
        let cfg2 = zarr_collection("data_path = \"x\"\n[collections.zarr.icechunk]\n");
        assert!(cfg2.validate().is_ok());
    }

    #[test]
    fn zarr_icechunk_rejects_multiple_version_selectors() {
        let cfg = zarr_collection(
            "data_path = \"x\"\n[collections.zarr.icechunk]\nbranch = \"main\"\ntag = \"v1\"\n",
        );
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn zarr_icechunk_rejects_empty_selector() {
        let cfg =
            zarr_collection("data_path = \"x\"\n[collections.zarr.icechunk]\nbranch = \"\"\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn zarr_rejects_absolute_or_traversal_path() {
        // An absolute `path` would make PathBuf::join discard `data_path`.
        let abs = zarr_collection("data_path = \"x\"\npath = \"/etc/passwd\"\n");
        assert!(abs.validate().is_err());
        let dotdot = zarr_collection("data_path = \"x\"\npath = \"../escape.zarr\"\n");
        assert!(dotdot.validate().is_err());
        // A normal relative sub-path is fine.
        let ok = zarr_collection("data_path = \"x\"\npath = \"sub/data.zarr\"\n");
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_keyword() {
        let cfg = collection_with("keywords = [\"ok\", \"  \"]\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_license_title() {
        let cfg = collection_with("[collections.license]\ntitle = \"\"\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_non_http_license_url() {
        let cfg = collection_with("[collections.license]\ntitle = \"X\"\nurl = \"ftp://x/y\"\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_accepts_valid_keywords_and_license() {
        let cfg = collection_with(
            "keywords = [\"radar\"]\n[collections.license]\ntitle = \"CC-BY-4.0\"\n",
        );
        assert!(cfg.validate().is_ok());
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
    fn watch_collections_dir_defaults_off_with_500ms_debounce() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            tmp.path(),
            "config.toml",
            r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "test"
title = "Test"
description = "A test"
"#,
        );
        let (config, _) = ServerConfig::from_file(path.to_str().unwrap()).unwrap();
        assert!(!config.server.watch_collections_dir);
        assert_eq!(config.server.watch_debounce_ms, 500);
    }

    #[test]
    fn watch_collections_dir_parses_explicit_values() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            tmp.path(),
            "config.toml",
            r#"
[server]
host = "127.0.0.1"
port = 8000
watch_collections_dir = true
watch_debounce_ms = 250

[[collections]]
id = "test"
title = "Test"
description = "A test"
"#,
        );
        let (config, _) = ServerConfig::from_file(path.to_str().unwrap()).unwrap();
        assert!(config.server.watch_collections_dir);
        assert_eq!(config.server.watch_debounce_ms, 250);
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
    fn extra_with_empty_parameter_rejected() {
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
name = "radar_fmi"
colormap = "radar_fmi"
parameter = ""
"#;
        let path = write_config(tmp.path(), "config.toml", config_toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(
                "Style bundle 'radar_multi': extra 'radar_fmi' has an empty 'parameter' field"
            ),
            "got: {err}"
        );
    }

    #[test]
    fn duplicate_extra_name_within_bundle_rejected() {
        // Two extras with the same name would silently overwrite each other
        // in build_styles' HashMap; catch at config load instead.
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
name = "radar_fmi"
colormap = "radar_fmi"

[[style_bundles.extras]]
name = "radar_fmi"
colormap = "radar_bookbinder"
"#;
        let path = write_config(tmp.path(), "config.toml", config_toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Style bundle 'radar_multi': duplicate extra name 'radar_fmi'"),
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
    fn style_bundles_in_per_collection_file_rejected() {
        // [[style_bundles]] inside a collections_dir file is dropped silently
        // by CollectionConfig (no field). Without this hard error the user
        // would see a confusing "not defined" validation failure instead.
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
            r#"
id = "radar"
title = "Radar"
description = "Radar"

[[style_bundles]]
id = "radar_multi"
[style_bundles.default]
colormap = "radar_dbz"
"#,
        );

        let path = tmp.path().join("config.toml");
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("radar.toml: [[style_bundles]] is not allowed in per-collection files"),
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

    // ---- postgis ------------------------------------------------------------

    fn nexus_postgis_toml() -> &'static str {
        r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "nexus-obs"
title = "Nexus Observations"
description = "FMI nexus weather observations"
engine_type = "postgis"
apis = ["edr", "features"]

[collections.postgis]
dsn_env = "NEXUS_DSN"

[collections.postgis.stations]
table = "weather.stations"
id_col = "wigos_id"
label_col = "name"
geom_col = "the_geom"
property_cols = ["territory"]

[collections.postgis.observations]
shape = "per_parameter"
station_fk_col = "wigos_id"
time_col = "time"
time_col_tz = "UTC"
value_col = "value"
geom_col = "the_geom"

[[collections.postgis.observations.tables]]
parameter = "t2m"
table = "weather.air_temperature"

[[collections.postgis.observations.tables]]
parameter = "ws_10m"
table = "weather.wind_speed"

[[collections.postgis.parameters]]
name = "t2m"
label = "2 m air temperature"
unit = "°C"
observed_property = "air_temperature"

[[collections.postgis.parameters]]
name = "ws_10m"
label = "10 m wind speed"
unit = "m/s"
observed_property = "wind_speed"
"#
    }

    #[test]
    fn postgis_nexus_per_parameter_parses_clean() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(tmp.path(), "config.toml", nexus_postgis_toml());
        let (config, _) = ServerConfig::from_file(path.to_str().unwrap()).unwrap();
        let c = &config.collections[0];
        assert_eq!(c.engine_type, "postgis");
        let pg = c.postgis.as_ref().unwrap();
        assert_eq!(pg.dsn_env, "NEXUS_DSN");
        assert_eq!(pg.observations.shape, "per_parameter");
        assert_eq!(pg.observations.tables.len(), 2);
        assert_eq!(pg.parameters.len(), 2);
    }

    #[test]
    fn postgis_engine_requires_postgis_section() {
        let tmp = TempDir::new().unwrap();
        let toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "nexus-obs"
title = "Nexus"
description = "Nexus"
engine_type = "postgis"
"#;
        let path = write_config(tmp.path(), "config.toml", toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("requires a [collections.postgis]"),
            "got: {err}"
        );
    }

    #[test]
    fn postgis_stations_optional_with_obs_geom_ok() {
        // Mode A: no [postgis.stations] block, but the per_parameter tables
        // carry a shared `geom_col` — locations are derived from observations.
        let tmp = TempDir::new().unwrap();
        let toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "obs"
title = "Obs"
description = "Obs"
engine_type = "postgis"
apis = ["edr", "features"]

[collections.postgis]
dsn_env = "OBS_DSN"

[collections.postgis.observations]
shape = "per_parameter"
station_fk_col = "wigos_id"
time_col = "time"
time_col_tz = "UTC"
value_col = "value"
geom_col = "the_geom"

[[collections.postgis.observations.tables]]
parameter = "t2m"
table = "public.airtemperature"

[[collections.postgis.parameters]]
name = "t2m"
label = "2 m air temperature"
unit = "degC"
"#;
        let path = write_config(tmp.path(), "config.toml", toml);
        let (config, _) = ServerConfig::from_file(path.to_str().unwrap()).unwrap();
        let pg = config.collections[0].postgis.as_ref().unwrap();
        assert!(pg.stations.is_none());
        assert!(pg.observations.obs_geom_available());
    }

    #[test]
    fn postgis_no_stations_no_geom_rejected() {
        // No stations table AND no observations geometry → cannot place any
        // location → hard config error.
        let tmp = TempDir::new().unwrap();
        let toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "obs"
title = "Obs"
description = "Obs"
engine_type = "postgis"

[collections.postgis]
dsn_env = "OBS_DSN"

[collections.postgis.observations]
shape = "long"
table = "public.obs"
station_fk_col = "station_id"
time_col = "time"
param_col = "param"
value_col = "value"

[[collections.postgis.parameters]]
name = "t2m"
label = "T"
unit = "degC"
"#;
        let path = write_config(tmp.path(), "config.toml", toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no [postgis.stations]") && err.contains("observations geometry"),
            "got: {err}"
        );
    }

    #[test]
    fn postgis_invalid_locations_window_rejected() {
        // A bad observations.locations_window must fail at config load, not at
        // the first metadata refresh. ("all" and valid ISO 8601 durations pass.)
        let tmp = TempDir::new().unwrap();
        let toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "obs"
title = "Obs"
description = "Obs"
engine_type = "postgis"

[collections.postgis]
dsn_env = "OBS_DSN"

[collections.postgis.observations]
shape = "per_parameter"
station_fk_col = "wigos_id"
time_col = "time"
time_col_tz = "UTC"
value_col = "value"
geom_col = "the_geom"
locations_window = "lol-not-a-duration"

[[collections.postgis.observations.tables]]
parameter = "t2m"
table = "public.airtemperature"

[[collections.postgis.parameters]]
name = "t2m"
label = "T"
unit = "degC"
"#;
        let path = write_config(tmp.path(), "config.toml", toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("locations_window"), "got: {err}");
    }

    #[test]
    fn postgis_requires_at_least_one_parameter() {
        let tmp = TempDir::new().unwrap();
        let toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "obs"
title = "Obs"
description = "Obs"
engine_type = "postgis"

[collections.postgis]
dsn_env = "OBS_DSN"

[collections.postgis.stations]
table = "public.stations"
id_col = "id"
label_col = "name"
geom_col = "geom"

[collections.postgis.observations]
shape = "long"
table = "public.obs"
station_fk_col = "station_id"
time_col = "time"
param_col = "param"
value_col = "value"
"#;
        let path = write_config(tmp.path(), "config.toml", toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("at least one [[parameters]]"), "got: {err}");
    }

    /// Combines both MC_ALLOW_INLINE_DB_URL paths (reject without opt-in,
    /// accept with it) into one test — cargo test runs tests in parallel and
    /// the process-wide env var can't be split across two test fns safely.
    #[test]
    fn postgis_literal_dsn_opt_in_behavior() {
        let tmp = TempDir::new().unwrap();
        let toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "obs"
title = "Obs"
description = "Obs"
engine_type = "postgis"

[collections.postgis]
dsn_env = "postgres://user:pass@localhost/obs"

[collections.postgis.stations]
table = "public.stations"
id_col = "id"
label_col = "name"
geom_col = "geom"

[collections.postgis.observations]
shape = "long"
table = "public.obs"
station_fk_col = "station_id"
time_col = "time"
param_col = "param"
value_col = "value"

[[collections.postgis.parameters]]
name = "t2m"
label = "Temp"
unit = "C"
"#;
        let path = write_config(tmp.path(), "config.toml", toml);

        // SAFETY: mutating process-wide env is racy across threads, but we
        // hold the guard across both calls before restoring.
        let guard = inline_db_url_guard();
        guard.set(None);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("literal database URL") || err.contains("MC_ALLOW_INLINE_DB_URL"),
            "without opt-in, expected rejection: {err}"
        );

        guard.set(Some("1"));
        let result = ServerConfig::from_file(path.to_str().unwrap());
        assert!(
            result.is_ok(),
            "with opt-in, expected accept: {:?}",
            result.err()
        );
    }

    // Tiny hand-rolled guard that serializes MC_ALLOW_INLINE_DB_URL mutations
    // without adding serial_test as a dep. Tests that touch this env var
    // MUST go through this guard — the lock is held for the test's duration.
    struct InlineDbUrlGuard<'a> {
        // Held for the duration of the guard; releases the mutex on Drop.
        #[allow(dead_code)]
        lock: std::sync::MutexGuard<'a, ()>,
    }
    impl InlineDbUrlGuard<'_> {
        fn set(&self, value: Option<&str>) {
            // SAFETY: lock is held for the duration of this guard.
            unsafe {
                match value {
                    Some(v) => std::env::set_var("MC_ALLOW_INLINE_DB_URL", v),
                    None => std::env::remove_var("MC_ALLOW_INLINE_DB_URL"),
                }
            }
        }
    }
    impl Drop for InlineDbUrlGuard<'_> {
        fn drop(&mut self) {
            // SAFETY: lock is held for the duration of this guard.
            unsafe {
                std::env::remove_var("MC_ALLOW_INLINE_DB_URL");
            }
        }
    }
    fn inline_db_url_guard() -> InlineDbUrlGuard<'static> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        InlineDbUrlGuard {
            lock: LOCK.lock().unwrap_or_else(|p| p.into_inner()),
        }
    }

    #[test]
    fn postgis_rejects_malicious_identifier() {
        let tmp = TempDir::new().unwrap();
        let toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "obs"
title = "Obs"
description = "Obs"
engine_type = "postgis"

[collections.postgis]
dsn_env = "OBS_DSN"

[collections.postgis.stations]
table = "public.stations"
id_col = "\"; DROP TABLE x;--"
label_col = "name"
geom_col = "geom"

[collections.postgis.observations]
shape = "long"
table = "public.obs"
station_fk_col = "station_id"
time_col = "time"
param_col = "param"
value_col = "value"

[[collections.postgis.parameters]]
name = "t2m"
label = "Temp"
unit = "C"
"#;
        let path = write_config(tmp.path(), "config.toml", toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not a valid SQL identifier"), "got: {err}");
    }

    #[test]
    fn postgis_wide_with_param_col_rejected() {
        let tmp = TempDir::new().unwrap();
        let toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "obs"
title = "Obs"
description = "Obs"
engine_type = "postgis"

[collections.postgis]
dsn_env = "OBS_DSN"

[collections.postgis.stations]
table = "public.stations"
id_col = "id"
label_col = "name"
geom_col = "geom"

[collections.postgis.observations]
shape = "wide"
table = "public.obs"
station_fk_col = "station_id"
time_col = "time"
param_col = "param"

[[collections.postgis.observations.columns]]
parameter = "t2m"
column = "temperature"

[[collections.postgis.parameters]]
name = "t2m"
label = "Temp"
unit = "C"
"#;
        let path = write_config(tmp.path(), "config.toml", toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("'wide' does not allow observations.param_col"),
            "got: {err}"
        );
    }

    #[test]
    fn postgis_long_with_columns_rejected() {
        let tmp = TempDir::new().unwrap();
        let toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "obs"
title = "Obs"
description = "Obs"
engine_type = "postgis"

[collections.postgis]
dsn_env = "OBS_DSN"

[collections.postgis.stations]
table = "public.stations"
id_col = "id"
label_col = "name"
geom_col = "geom"

[collections.postgis.observations]
shape = "long"
table = "public.obs"
station_fk_col = "station_id"
time_col = "time"
param_col = "param"
value_col = "value"

[[collections.postgis.observations.columns]]
parameter = "t2m"
column = "temperature"

[[collections.postgis.parameters]]
name = "t2m"
label = "Temp"
unit = "C"
"#;
        let path = write_config(tmp.path(), "config.toml", toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("'long' does not allow [[observations.columns]]"),
            "got: {err}"
        );
    }

    #[test]
    fn postgis_per_parameter_with_table_field_rejected() {
        let tmp = TempDir::new().unwrap();
        let toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "obs"
title = "Obs"
description = "Obs"
engine_type = "postgis"

[collections.postgis]
dsn_env = "OBS_DSN"

[collections.postgis.stations]
table = "public.stations"
id_col = "id"
label_col = "name"
geom_col = "geom"

[collections.postgis.observations]
shape = "per_parameter"
table = "public.obs"
station_fk_col = "station_id"
time_col = "time"
value_col = "value"

[[collections.postgis.observations.tables]]
parameter = "t2m"
table = "public.temp"

[[collections.postgis.parameters]]
name = "t2m"
label = "Temp"
unit = "C"
"#;
        let path = write_config(tmp.path(), "config.toml", toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("'per_parameter' does not allow observations.table"),
            "got: {err}"
        );
    }

    #[test]
    fn postgis_unknown_shape_rejected() {
        let tmp = TempDir::new().unwrap();
        let toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "obs"
title = "Obs"
description = "Obs"
engine_type = "postgis"

[collections.postgis]
dsn_env = "OBS_DSN"

[collections.postgis.stations]
table = "public.stations"
id_col = "id"
label_col = "name"
geom_col = "geom"

[collections.postgis.observations]
shape = "bogus"

[[collections.postgis.parameters]]
name = "t2m"
label = "Temp"
unit = "C"
"#;
        let path = write_config(tmp.path(), "config.toml", toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("'bogus'"), "got: {err}");
    }

    #[test]
    fn postgis_parameter_not_mapped_in_columns_rejected() {
        let tmp = TempDir::new().unwrap();
        let toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "obs"
title = "Obs"
description = "Obs"
engine_type = "postgis"

[collections.postgis]
dsn_env = "OBS_DSN"

[collections.postgis.stations]
table = "public.stations"
id_col = "id"
label_col = "name"
geom_col = "geom"

[collections.postgis.observations]
shape = "wide"
table = "public.obs"
station_fk_col = "station_id"
time_col = "time"

[[collections.postgis.observations.columns]]
parameter = "t2m"
column = "temperature"

[[collections.postgis.parameters]]
name = "ws_10m"
label = "Wind"
unit = "m/s"
"#;
        let path = write_config(tmp.path(), "config.toml", toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("'ws_10m'") && err.contains("not mapped"),
            "got: {err}"
        );
    }

    #[test]
    fn postgis_section_without_postgis_engine_rejected() {
        let tmp = TempDir::new().unwrap();
        let toml = r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "obs"
title = "Obs"
description = "Obs"

[collections.postgis]
dsn_env = "OBS_DSN"

[collections.postgis.stations]
table = "public.stations"
id_col = "id"
label_col = "name"
geom_col = "geom"

[collections.postgis.observations]
shape = "long"
table = "public.obs"
station_fk_col = "station_id"
time_col = "time"
param_col = "param"
value_col = "value"

[[collections.postgis.parameters]]
name = "t2m"
label = "Temp"
unit = "C"
"#;
        let path = write_config(tmp.path(), "config.toml", toml);
        let err = ServerConfig::from_file(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("[collections.postgis] is set but engine_type"),
            "got: {err}"
        );
    }

    #[test]
    fn is_valid_sql_identifier_cases() {
        // accepts
        assert!(is_valid_sql_identifier("_"));
        assert!(is_valid_sql_identifier("a"));
        assert!(is_valid_sql_identifier("a1"));
        assert!(is_valid_sql_identifier("wigos_id"));
        assert!(is_valid_sql_identifier(&"a".repeat(63)));

        // rejects
        assert!(!is_valid_sql_identifier(""));
        assert!(!is_valid_sql_identifier(&"a".repeat(64)));
        assert!(!is_valid_sql_identifier("1abc"));
        assert!(!is_valid_sql_identifier("a\"b"));
        assert!(!is_valid_sql_identifier("a\0b"));
        assert!(!is_valid_sql_identifier("\"; DROP TABLE x;--"));
        assert!(!is_valid_sql_identifier("a b"));
        assert!(!is_valid_sql_identifier("a-b"));
    }

    #[test]
    fn validate_stations_where_clause_accepts_simple_filters() {
        for ok in [
            "active = true",
            "quality_code IN (1, 2, 3)",
            "country_code = 'FI'",
            "obs_count > 0 AND station_type != 'mobile'",
        ] {
            validate_stations_where_clause("c", ok)
                .unwrap_or_else(|e| panic!("expected OK for '{ok}', got {e:?}"));
        }
    }

    #[test]
    fn validate_stations_where_clause_rejects_injection() {
        // Each of these should surface as a config error so a typo or
        // compromised config file cannot chain a destructive statement.
        for bad in [
            "1=1; DROP TABLE stations;--",
            "1=1; DELETE FROM stations",
            "1=1 -- and more",
            "1=1 /* comment */",
            "status = 'ok' */",
            "DROP TABLE stations",
            "x = 1 OR DELETE x",
            "TRUNCATE stations",
            "grant select to public",
            "1; copy stations to program 'evil'",
            // Data-exfil / dynamic-execution — flagged in PR review.
            "territory = 'x' UNION SELECT password FROM admin_users",
            "1=1 EXECUTE 'DROP TABLE stations'",
            "1=1 call do_something()",
            "1=1 perform evil()",
            // Correlated subquery — needs SELECT/FROM in the blocklist.
            "territory = (SELECT territory FROM stations LIMIT 1)",
            // Whitespace bypass — verbs separated by non-space whitespace.
            "active\nunion\nselect 1",
            "active\tdelete\tfrom stations",
        ] {
            assert!(
                validate_stations_where_clause("c", bad).is_err(),
                "expected error for '{bad}'"
            );
        }
    }

    #[test]
    fn validate_stations_where_clause_rejects_oversize() {
        let long = "a = ".to_string() + &"1".repeat(520);
        assert!(validate_stations_where_clause("c", &long).is_err());
    }

    #[test]
    fn validate_qualified_table_cases() {
        assert_eq!(
            validate_qualified_table("stations"),
            Ok(("public", "stations"))
        );
        assert_eq!(
            validate_qualified_table("weather.stations"),
            Ok(("weather", "stations"))
        );
        assert!(validate_qualified_table("a.b.c").is_err());
        assert!(validate_qualified_table(".bare").is_err());
        assert!(validate_qualified_table("bare.").is_err());
        assert!(validate_qualified_table("1schema.tbl").is_err());
    }
}
