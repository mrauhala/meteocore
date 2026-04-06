//! STAC API client for discovering GeoTIFF assets.
//!
//! Fetches items from a STAC API endpoint, extracting timestamps and asset URLs.
//! Asset URLs are validated against an allowlist for SSRF protection.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use url::Url;

/// Maximum items per page when querying the STAC API.
const STAC_PAGE_LIMIT: u32 = 100;

/// Maximum number of pages to follow when paginating.
/// With 100 items/page, this allows up to 100K items on initial bootstrap.
/// Subsequent incremental polls typically need only 1-2 pages.
const STAC_MAX_PAGES: u32 = 1000;

/// HTTP request timeout for individual STAC API calls.
const STAC_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Total timeout budget for the entire pagination loop.
/// Generous for initial bootstrap; incremental polls finish quickly.
const STAC_TOTAL_TIMEOUT: Duration = Duration::from_secs(600);

/// A single item discovered from a STAC catalog.
#[derive(Debug, Clone)]
pub struct StacItem {
    pub datetime: DateTime<Utc>,
    pub asset_url: String,
    pub file_size: Option<u64>,
    pub item_id: String,
    pub bbox: Option<[f64; 4]>,
}

/// Extent metadata from a STAC collection (no item fetching needed).
#[derive(Debug, Clone)]
pub struct StacExtent {
    pub spatial_bbox: Option<[f64; 4]>,
    pub temporal_start: Option<DateTime<Utc>>,
    pub temporal_end: Option<DateTime<Utc>>,
}

/// Client for querying a STAC API items endpoint.
///
/// Uses an async reqwest::Client internally, bridged to sync via `block_in_place`.
/// This avoids the "cannot drop runtime in async context" panic that occurs with
/// `reqwest::blocking::Client` when used inside a tokio runtime.
#[derive(Debug)]
pub struct StacClient {
    items_url: Url,
    /// Collection URL (items_url with `/items` stripped).
    collection_url: Url,
    asset_key: String,
    asset_allowlist: Vec<Url>,
    http: Arc<reqwest::Client>,
}

impl StacClient {
    /// Create a new STAC client.
    ///
    /// `items_url` must be a valid URL pointing to a STAC items endpoint.
    /// `asset_allowlist` is a list of URL prefixes that asset URLs must match.
    pub fn new(
        items_url: &str,
        asset_key: &str,
        asset_allowlist: Vec<String>,
    ) -> Result<Self, DataServerError> {
        let parsed_url = Url::parse(items_url).map_err(|e| {
            DataServerError::Engine(format!("Invalid STAC URL '{}': {}", items_url, e))
        })?;

        if parsed_url.scheme() != "https" && parsed_url.scheme() != "http" {
            return Err(DataServerError::Engine(format!(
                "STAC URL must use http or https scheme, got '{}'",
                parsed_url.scheme()
            )));
        }

        let http = reqwest::Client::builder()
            .timeout(STAC_REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("MeteoCore/0.1")
            .build()
            .map_err(|e| DataServerError::Engine(format!("Failed to build HTTP client: {}", e)))?;

        // Validate allowlist entries are valid http/https URLs
        let mut parsed_allowlist = Vec::with_capacity(asset_allowlist.len());
        for entry in &asset_allowlist {
            let parsed = Url::parse(entry).map_err(|e| {
                DataServerError::Engine(format!("Invalid allowlist URL '{}': {}", entry, e))
            })?;
            if parsed.scheme() != "http" && parsed.scheme() != "https" {
                return Err(DataServerError::Engine(format!(
                    "Allowlist URL must use http or https scheme, got '{}' in '{}'",
                    parsed.scheme(),
                    entry
                )));
            }
            parsed_allowlist.push(parsed);
        }

        // Derive collection URL by stripping /items suffix
        let items_str = parsed_url.as_str();
        let collection_str = items_str
            .strip_suffix("/items")
            .or_else(|| items_str.strip_suffix("/items/"))
            .unwrap_or(items_str);
        let collection_url = Url::parse(collection_str).map_err(|e| {
            DataServerError::Engine(format!("Failed to derive collection URL: {}", e))
        })?;

        Ok(StacClient {
            items_url: parsed_url,
            collection_url,
            asset_key: asset_key.to_string(),
            asset_allowlist: parsed_allowlist,
            http: Arc::new(http),
        })
    }

    /// Get a clone of the Arc-wrapped HTTP client for use in DataSource::HttpDirect.
    pub fn http_client(&self) -> Arc<reqwest::Client> {
        self.http.clone()
    }

    /// Bridge async to sync. Uses `block_in_place` when inside a tokio runtime,
    /// or creates a temporary runtime otherwise. Same pattern as DataStore.
    fn block_on<F, T>(&self, future: F) -> Result<T, DataServerError>
    where
        F: std::future::Future<Output = Result<T, DataServerError>>,
    {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
            Err(_) => {
                let rt = tokio::runtime::Runtime::new().map_err(|e| {
                    DataServerError::Engine(format!("Failed to create runtime: {}", e))
                })?;
                rt.block_on(future)
            }
        }
    }

    /// Fetch the collection extent from the STAC collection endpoint.
    ///
    /// Returns spatial bbox and temporal interval without fetching any items.
    /// This is used at startup to seed the catalog with extent information.
    pub fn fetch_extent(&self) -> Result<StacExtent, DataServerError> {
        tracing::debug!("STAC collection fetch: {}", self.collection_url);
        let url = self.collection_url.as_str().to_string();
        let http = self.http.clone();
        let body: serde_json::Value = self.block_on(async {
            let response = http.get(&url).send().await.map_err(|e| {
                tracing::warn!("STAC collection request to {} failed: {}", url, e);
                DataServerError::Engine("STAC collection request failed".into())
            })?;

            if !response.status().is_success() {
                tracing::warn!(
                    "STAC collection {} returned HTTP {}",
                    url,
                    response.status()
                );
                return Err(DataServerError::Engine(
                    "STAC collection request failed".into(),
                ));
            }

            response.json().await.map_err(|e| {
                tracing::warn!("STAC collection response from {} invalid JSON: {}", url, e);
                DataServerError::Engine("STAC collection response invalid".into())
            })
        })?;

        // Extract spatial bbox: extent.spatial.bbox[0] = [west, south, east, north]
        let spatial_bbox = body["extent"]["spatial"]["bbox"]
            .as_array()
            .and_then(|bboxes| bboxes.first())
            .and_then(|bbox| bbox.as_array())
            .and_then(|arr| {
                if arr.len() >= 4 {
                    Some([
                        arr[0].as_f64()?,
                        arr[1].as_f64()?,
                        arr[2].as_f64()?,
                        arr[3].as_f64()?,
                    ])
                } else {
                    None
                }
            });

        // Extract temporal interval: extent.temporal.interval[0] = [start, end|null]
        let interval = body["extent"]["temporal"]["interval"]
            .as_array()
            .and_then(|intervals| intervals.first())
            .and_then(|i| i.as_array());

        let temporal_start = interval
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));

        let temporal_end = interval
            .and_then(|arr| arr.get(1))
            .and_then(|v| v.as_str()) // null means ongoing
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));

        Ok(StacExtent {
            spatial_bbox,
            temporal_start,
            temporal_end,
        })
    }

    /// Fetch items from the STAC API, optionally filtered by time range.
    ///
    /// Returns items sorted by datetime (ascending). Follows pagination up to
    /// `STAC_MAX_PAGES` pages. Stops early if `max_items` is reached.
    pub fn fetch_items(
        &self,
        time_filter: Option<(DateTime<Utc>, DateTime<Utc>)>,
        max_items: Option<usize>,
    ) -> Result<Vec<StacItem>, DataServerError> {
        let mut items = Vec::new();
        let mut url = self.build_initial_url(time_filter, max_items);
        let mut pages = 0;
        let start_time = Instant::now();

        loop {
            if pages >= STAC_MAX_PAGES {
                tracing::warn!(
                    "STAC pagination limit reached ({} pages), stopping",
                    STAC_MAX_PAGES
                );
                break;
            }

            if start_time.elapsed() > STAC_TOTAL_TIMEOUT {
                tracing::warn!(
                    "STAC total timeout reached ({:?}), stopping after {} pages",
                    STAC_TOTAL_TIMEOUT,
                    pages
                );
                break;
            }

            tracing::debug!("STAC fetch: {}", url);
            let url_str = url.as_str().to_string();
            let http = self.http.clone();
            let body: serde_json::Value = self.block_on(async {
                let retry_delays = [
                    Duration::from_millis(100),
                    Duration::from_millis(200),
                    Duration::from_millis(400),
                ];
                let max_attempts = retry_delays.len() + 1; // 1 initial + 3 retries

                let mut last_err = None;
                for attempt in 0..max_attempts {
                    let result = http.get(&url_str).send().await;
                    match result {
                        Ok(response) => {
                            let status = response.status();
                            if status.is_success() {
                                return response.json().await.map_err(|e| {
                                    tracing::warn!(
                                        "STAC response from {} is not valid JSON: {}",
                                        url_str,
                                        e
                                    );
                                    DataServerError::Engine("STAC response invalid".into())
                                });
                            }
                            // 4xx: do not retry (client error)
                            if status.is_client_error() {
                                tracing::warn!(
                                    "STAC API {} returned HTTP {} (not retrying)",
                                    url_str,
                                    status
                                );
                                return Err(DataServerError::Engine(
                                    "STAC metadata request failed".into(),
                                ));
                            }
                            // 5xx: retry
                            tracing::warn!(
                                "STAC API {} returned HTTP {} (attempt {}/{})",
                                url_str,
                                status,
                                attempt + 1,
                                max_attempts
                            );
                            last_err = Some(DataServerError::Engine(
                                "STAC metadata request failed".into(),
                            ));
                        }
                        Err(e) => {
                            tracing::warn!(
                                "STAC request to {} failed: {} (attempt {}/{})",
                                url_str,
                                e,
                                attempt + 1,
                                max_attempts
                            );
                            last_err = Some(DataServerError::Engine(
                                "STAC metadata request failed".into(),
                            ));
                        }
                    }

                    // Sleep before retry (not after last attempt)
                    if attempt < retry_delays.len() {
                        tokio::time::sleep(retry_delays[attempt]).await;
                    }
                }

                Err(last_err.unwrap_or_else(|| {
                    DataServerError::Engine("STAC metadata request failed".into())
                }))
            })?;

            // Parse features array
            let features = body["features"].as_array().ok_or_else(|| {
                DataServerError::Engine("STAC response missing 'features' array".to_string())
            })?;

            if features.is_empty() {
                break;
            }

            for feature in features {
                if let Some(item) = self.parse_item(feature) {
                    items.push(item);
                }
            }

            // Check if we have enough items
            if let Some(max) = max_items {
                if items.len() >= max {
                    items.truncate(max);
                    break;
                }
            }

            // Follow pagination
            match self.extract_next_link(&body) {
                Some(next_url) => {
                    url = next_url;
                    pages += 1;
                }
                None => break,
            }
        }

        // Sort by datetime ascending
        items.sort_by_key(|item| item.datetime);

        Ok(items)
    }

    /// Build the initial request URL with query parameters.
    fn build_initial_url(
        &self,
        time_filter: Option<(DateTime<Utc>, DateTime<Utc>)>,
        max_items: Option<usize>,
    ) -> Url {
        let mut url = self.items_url.clone();

        let limit = max_items
            .map(|m| (m as u32).min(STAC_PAGE_LIMIT))
            .unwrap_or(STAC_PAGE_LIMIT);

        url.query_pairs_mut()
            .append_pair("limit", &limit.to_string());

        if let Some((start, end)) = time_filter {
            let datetime_param = format!(
                "{}/{}",
                start.format("%Y-%m-%dT%H:%M:%SZ"),
                end.format("%Y-%m-%dT%H:%M:%SZ")
            );
            url.query_pairs_mut()
                .append_pair("datetime", &datetime_param);
        }

        url
    }

    /// Parse a single GeoJSON feature into a StacItem.
    /// Returns None if the feature is missing required fields or the asset URL
    /// is not in the allowlist.
    fn parse_item(&self, feature: &serde_json::Value) -> Option<StacItem> {
        let item_id = feature["id"].as_str().unwrap_or("unknown").to_string();

        // Extract datetime from properties (fall back to start_datetime per STAC spec)
        let datetime_str = feature["properties"]["datetime"]
            .as_str()
            .or_else(|| feature["properties"]["start_datetime"].as_str());
        let datetime_str = match datetime_str {
            Some(s) => s,
            None => {
                tracing::warn!("STAC item '{}' has no datetime, skipping", item_id);
                return None;
            }
        };
        let datetime = DateTime::parse_from_rfc3339(datetime_str)
            .ok()
            .or_else(|| {
                // Try without timezone (assume UTC)
                chrono::NaiveDateTime::parse_from_str(datetime_str, "%Y-%m-%dT%H:%M:%SZ")
                    .ok()
                    .map(|dt| dt.and_utc().fixed_offset())
            })?
            .with_timezone(&Utc);

        // Extract asset URL
        let asset = &feature["assets"][&self.asset_key];
        let asset_url = asset["href"].as_str()?;

        // Validate asset URL scheme (reject file://, data:, gopher://, etc.)
        if !asset_url.starts_with("http://") && !asset_url.starts_with("https://") {
            tracing::warn!(
                "STAC item '{}': asset URL has invalid scheme, skipping",
                item_id,
            );
            return None;
        }

        // SSRF protection: validate asset URL against allowlist
        if !self.is_url_allowed(asset_url) {
            tracing::warn!(
                "STAC item '{}': asset URL not in allowlist, skipping",
                item_id,
            );
            return None;
        }

        let file_size = asset["file:size"]
            .as_u64()
            .or_else(|| asset["content-length"].as_u64());

        // Extract bbox from top-level feature (standard STAC: [west, south, east, north])
        let bbox = feature["bbox"].as_array().and_then(|arr| {
            if arr.len() >= 4 {
                Some([
                    arr[0].as_f64()?,
                    arr[1].as_f64()?,
                    arr[2].as_f64()?,
                    arr[3].as_f64()?,
                ])
            } else {
                None
            }
        });

        Some(StacItem {
            datetime,
            asset_url: asset_url.to_string(),
            file_size,
            item_id,
            bbox,
        })
    }

    /// Extract the "next" pagination link from a STAC response.
    /// Validates that the next URL is same-origin to prevent redirect attacks.
    fn extract_next_link(&self, body: &serde_json::Value) -> Option<Url> {
        let links = body["links"].as_array()?;

        for link in links {
            if link["rel"].as_str() == Some("next") {
                let href = link["href"].as_str()?;
                let next_url = Url::parse(href).ok()?;

                if !self.is_same_origin(&next_url) {
                    tracing::warn!(
                        "STAC pagination: next link '{}' is cross-origin, stopping",
                        href
                    );
                    return None;
                }

                return Some(next_url);
            }
        }

        None
    }

    /// Check if a URL matches any entry in the asset allowlist.
    /// Compares scheme, host, and port exactly, then checks that the asset path
    /// starts with the allowlist entry's path. This prevents hostname confusion
    /// (e.g., allowlist "example.com" matching "example.comevil").
    ///
    /// Path handling: if the allowlist entry has an empty path (e.g., "https://host.com"),
    /// it is normalized to "/" so that it matches all paths on that host (since all valid
    /// URLs have paths starting with "/"). To restrict to a specific prefix, include it
    /// in the allowlist entry (e.g., "https://host.com/data/").
    fn is_url_allowed(&self, url: &str) -> bool {
        let parsed = match Url::parse(url) {
            Ok(u) => u,
            Err(_) => return false,
        };

        self.asset_allowlist.iter().any(|allowed| {
            let allowed_path = if allowed.path().is_empty() {
                "/"
            } else {
                allowed.path()
            };
            parsed.scheme() == allowed.scheme()
                && parsed.host() == allowed.host()
                && parsed.port() == allowed.port()
                && parsed.path().starts_with(allowed_path)
        })
    }

    /// Check if a URL has the same scheme+host as the items URL.
    fn is_same_origin(&self, url: &Url) -> bool {
        url.scheme() == self.items_url.scheme()
            && url.host() == self.items_url.host()
            && url.port() == self.items_url.port()
    }

    // --- Direct HTTP methods for asset fetching ---
    // These bypass object_store which URL-encodes path components
    // (breaking Ceph RGW URLs with colons in the path).

    /// Fetch the file size via a HEAD request.
    pub fn head_asset(&self, url: &str) -> Result<u64, DataServerError> {
        let url_owned = url.to_string();
        let http = self.http.clone();
        self.block_on(async {
            let resp = http.head(&url_owned).send().await.map_err(|e| {
                tracing::warn!("HEAD failed for '{}': {}", url_owned, e);
                DataServerError::Engine("Failed to fetch remote file metadata".into())
            })?;
            if !resp.status().is_success() {
                tracing::warn!("HEAD returned {} for '{}'", resp.status(), url_owned);
                return Err(DataServerError::Engine(
                    "Failed to fetch remote file metadata".into(),
                ));
            }
            let size = resp
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            Ok(size)
        })
    }

    /// Fetch a byte range from an asset URL.
    pub fn get_range(
        &self,
        url: &str,
        start: u64,
        end: u64,
    ) -> Result<bytes::Bytes, DataServerError> {
        let range_header = format!("bytes={}-{}", start, end);
        let url_owned = url.to_string();
        let http = self.http.clone();
        self.block_on(async {
            let resp = http
                .get(&url_owned)
                .header(reqwest::header::RANGE, &range_header)
                .send()
                .await
                .map_err(|e| {
                    tracing::warn!("Range read failed for '{}': {}", url_owned, e);
                    DataServerError::Engine("Failed to read remote file".into())
                })?;
            if !resp.status().is_success() {
                tracing::warn!("Range read returned {} for '{}'", resp.status(), url_owned);
                return Err(DataServerError::Engine("Failed to read remote file".into()));
            }
            let data = resp.bytes().await.map_err(|e| {
                tracing::warn!("Failed to read body from '{}': {}", url_owned, e);
                DataServerError::Engine("Failed to read remote file".into())
            })?;
            Ok(data)
        })
    }

    /// Download an entire asset.
    pub fn get_asset(&self, url: &str) -> Result<bytes::Bytes, DataServerError> {
        let url_owned = url.to_string();
        let http = self.http.clone();
        self.block_on(async {
            let resp = http.get(&url_owned).send().await.map_err(|e| {
                tracing::warn!("Download failed for '{}': {:?}", url_owned, e);
                DataServerError::Engine("Failed to download remote file".into())
            })?;
            if !resp.status().is_success() {
                tracing::warn!("Download returned {} for '{}'", resp.status(), url_owned);
                return Err(DataServerError::Engine(
                    "Failed to download remote file".into(),
                ));
            }
            let data = resp.bytes().await.map_err(|e| {
                tracing::warn!("Failed to read download body from '{}': {}", url_owned, e);
                DataServerError::Engine("Failed to download remote file".into())
            })?;
            Ok(data)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stac_item() {
        let client = StacClient::new(
            "https://api.example.com/collections/radar/items",
            "data",
            vec!["https://thredds.example.com/".to_string()],
        )
        .unwrap();

        let feature = serde_json::json!({
            "id": "radar-20260325T1200Z",
            "type": "Feature",
            "properties": {
                "datetime": "2026-03-25T12:00:00Z"
            },
            "assets": {
                "data": {
                    "href": "https://thredds.example.com/radar/20260325T1200Z.tif",
                    "file:size": 1048576
                }
            }
        });

        let item = client.parse_item(&feature).unwrap();
        assert_eq!(item.item_id, "radar-20260325T1200Z");
        assert_eq!(item.datetime.to_rfc3339(), "2026-03-25T12:00:00+00:00");
        assert_eq!(
            item.asset_url,
            "https://thredds.example.com/radar/20260325T1200Z.tif"
        );
        assert_eq!(item.file_size, Some(1048576));
    }

    #[test]
    fn ssrf_blocked() {
        let client = StacClient::new(
            "https://api.example.com/collections/radar/items",
            "data",
            vec!["https://thredds.example.com/".to_string()],
        )
        .unwrap();

        let feature = serde_json::json!({
            "id": "evil",
            "type": "Feature",
            "properties": {
                "datetime": "2026-03-25T12:00:00Z"
            },
            "assets": {
                "data": {
                    "href": "http://169.254.169.254/latest/meta-data/",
                    "file:size": 100
                }
            }
        });

        assert!(client.parse_item(&feature).is_none());
    }

    #[test]
    fn pagination_next_link() {
        let client = StacClient::new(
            "https://api.example.com/collections/radar/items",
            "data",
            vec![],
        )
        .unwrap();

        let body = serde_json::json!({
            "features": [],
            "links": [
                {
                    "rel": "next",
                    "href": "https://api.example.com/collections/radar/items?offset=100"
                }
            ]
        });

        let next = client.extract_next_link(&body).unwrap();
        assert_eq!(
            next.as_str(),
            "https://api.example.com/collections/radar/items?offset=100"
        );
    }

    #[test]
    fn pagination_cross_origin_blocked() {
        let client = StacClient::new(
            "https://api.example.com/collections/radar/items",
            "data",
            vec![],
        )
        .unwrap();

        let body = serde_json::json!({
            "features": [],
            "links": [
                {
                    "rel": "next",
                    "href": "https://evil.example.com/collections/radar/items?offset=100"
                }
            ]
        });

        assert!(client.extract_next_link(&body).is_none());
    }

    #[test]
    fn missing_datetime_skipped() {
        let client = StacClient::new(
            "https://api.example.com/collections/radar/items",
            "data",
            vec!["https://thredds.example.com/".to_string()],
        )
        .unwrap();

        let feature = serde_json::json!({
            "id": "no-datetime",
            "type": "Feature",
            "properties": {},
            "assets": {
                "data": {
                    "href": "https://thredds.example.com/radar/file.tif"
                }
            }
        });

        assert!(client.parse_item(&feature).is_none());
    }

    #[test]
    fn missing_asset_key_skipped() {
        let client = StacClient::new(
            "https://api.example.com/collections/radar/items",
            "data",
            vec!["https://thredds.example.com/".to_string()],
        )
        .unwrap();

        let feature = serde_json::json!({
            "id": "wrong-key",
            "type": "Feature",
            "properties": {
                "datetime": "2026-03-25T12:00:00Z"
            },
            "assets": {
                "thumbnail": {
                    "href": "https://thredds.example.com/radar/thumb.png"
                }
            }
        });

        assert!(client.parse_item(&feature).is_none());
    }

    #[test]
    fn build_url_with_time_filter() {
        let client = StacClient::new(
            "https://api.example.com/collections/radar/items",
            "data",
            vec![],
        )
        .unwrap();

        let start = "2026-03-25T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let end = "2026-03-25T12:00:00Z".parse::<DateTime<Utc>>().unwrap();

        let url = client.build_initial_url(Some((start, end)), None);
        let query: String = url.query().unwrap_or("").to_string();
        assert!(query.contains("datetime=2026-03-25T10%3A00%3A00Z%2F2026-03-25T12%3A00%3A00Z"));
        assert!(query.contains("limit=100"));
        assert!(!query.contains("sortby")); // sortby removed — client-side sort only
    }

    #[test]
    fn build_url_with_max_items() {
        let client = StacClient::new(
            "https://api.example.com/collections/radar/items",
            "data",
            vec![],
        )
        .unwrap();

        let url = client.build_initial_url(None, Some(10));
        let query: String = url.query().unwrap_or("").to_string();
        assert!(query.contains("limit=10"));
    }

    #[test]
    fn is_url_allowed_checks_prefix() {
        let client = StacClient::new(
            "https://api.example.com/items",
            "data",
            vec![
                "https://thredds.met.no/".to_string(),
                "https://cdn.example.com/data/".to_string(),
            ],
        )
        .unwrap();

        assert!(client.is_url_allowed("https://thredds.met.no/radar/file.tif"));
        assert!(client.is_url_allowed("https://cdn.example.com/data/file.tif"));
        assert!(!client.is_url_allowed("https://evil.com/file.tif"));
        assert!(!client.is_url_allowed("http://thredds.met.no/file.tif")); // wrong scheme
    }

    #[test]
    fn same_origin_check() {
        let client = StacClient::new(
            "https://api.example.com/collections/radar/items",
            "data",
            vec![],
        )
        .unwrap();

        let same = Url::parse("https://api.example.com/other/path").unwrap();
        assert!(client.is_same_origin(&same));

        let different_host = Url::parse("https://other.example.com/path").unwrap();
        assert!(!client.is_same_origin(&different_host));

        let different_scheme = Url::parse("http://api.example.com/path").unwrap();
        assert!(!client.is_same_origin(&different_scheme));
    }

    #[test]
    fn start_datetime_fallback() {
        let client = StacClient::new(
            "https://api.example.com/collections/radar/items",
            "data",
            vec!["https://thredds.example.com/".to_string()],
        )
        .unwrap();

        let feature = serde_json::json!({
            "id": "range-item",
            "type": "Feature",
            "properties": {
                "datetime": null,
                "start_datetime": "2026-03-25T12:00:00Z",
                "end_datetime": "2026-03-25T13:00:00Z"
            },
            "assets": {
                "data": {
                    "href": "https://thredds.example.com/radar/file.tif"
                }
            }
        });

        let item = client.parse_item(&feature).unwrap();
        assert_eq!(item.datetime.to_rfc3339(), "2026-03-25T12:00:00+00:00");
    }

    #[test]
    fn url_hostname_confusion_blocked() {
        let client = StacClient::new(
            "https://api.example.com/items",
            "data",
            vec!["https://example.com/".to_string()],
        )
        .unwrap();

        // "example.comevil" should NOT match "example.com"
        assert!(!client.is_url_allowed("https://example.comevil/file.tif"));
        // But "example.com/subpath" should match
        assert!(client.is_url_allowed("https://example.com/subpath/file.tif"));
    }

    #[test]
    fn invalid_scheme_blocked() {
        let client = StacClient::new(
            "https://api.example.com/collections/radar/items",
            "data",
            vec!["https://thredds.example.com/".to_string()],
        )
        .unwrap();

        let feature = serde_json::json!({
            "id": "file-scheme",
            "type": "Feature",
            "properties": {
                "datetime": "2026-03-25T12:00:00Z"
            },
            "assets": {
                "data": {
                    "href": "file:///etc/passwd"
                }
            }
        });

        assert!(client.parse_item(&feature).is_none());
    }

    #[test]
    fn invalid_allowlist_entry_rejected() {
        let result = StacClient::new(
            "https://api.example.com/items",
            "data",
            vec!["not-a-url".to_string()],
        );
        assert!(result.is_err());
    }

    #[test]
    fn non_http_allowlist_entry_rejected() {
        let result = StacClient::new(
            "https://api.example.com/items",
            "data",
            vec!["ftp://files.example.com/".to_string()],
        );
        assert!(result.is_err());
    }

    #[test]
    fn parse_stac_item_with_bbox() {
        let client = StacClient::new(
            "https://api.example.com/collections/radar/items",
            "data",
            vec!["https://thredds.example.com/".to_string()],
        )
        .unwrap();

        let feature = serde_json::json!({
            "id": "radar-bbox",
            "type": "Feature",
            "bbox": [10.0, 55.0, 30.0, 72.0],
            "properties": {
                "datetime": "2026-03-25T12:00:00Z"
            },
            "assets": {
                "data": {
                    "href": "https://thredds.example.com/radar/file.tif",
                    "file:size": 2048
                }
            }
        });

        let item = client.parse_item(&feature).unwrap();
        let bbox = item.bbox.expect("bbox should be Some");
        assert_eq!(bbox, [10.0, 55.0, 30.0, 72.0]);
    }

    #[test]
    fn parse_stac_item_without_bbox() {
        let client = StacClient::new(
            "https://api.example.com/collections/radar/items",
            "data",
            vec!["https://thredds.example.com/".to_string()],
        )
        .unwrap();

        let feature = serde_json::json!({
            "id": "radar-no-bbox",
            "type": "Feature",
            "properties": {
                "datetime": "2026-03-25T12:00:00Z"
            },
            "assets": {
                "data": {
                    "href": "https://thredds.example.com/radar/file.tif"
                }
            }
        });

        let item = client.parse_item(&feature).unwrap();
        assert!(item.bbox.is_none());
    }

    #[test]
    fn redirect_policy_is_none() {
        // Verify client builds successfully with redirect disabled
        let client = StacClient::new(
            "https://api.example.com/items",
            "data",
            vec!["https://data.example.com/".to_string()],
        );
        assert!(client.is_ok());
    }
}
