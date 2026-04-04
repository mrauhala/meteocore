use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, NaiveDateTime, Utc};
use ds_core::error::DataServerError;
use regex::Regex;

use crate::reader::{DataSource, TiffMetadata};

/// Maximum filename length to prevent abuse.
const MAX_FILENAME_LENGTH: usize = 255;

/// Lightweight stub for STAC entries that haven't had their GeoTIFF metadata loaded yet.
#[derive(Debug, Clone)]
pub struct StacStub {
    pub bbox: Option<[f64; 4]>,
    pub asset_url: String,
}

/// The loading state of a file entry.
///
/// Non-STAC entries (local/remote) are always `Loaded` since metadata is parsed
/// during the scan. STAC entries start as `Stub` and transition to `Loaded` when
/// GeoTIFF metadata is fetched on demand.
#[derive(Debug, Clone)]
pub enum FileState {
    /// STAC stub: only STAC metadata available, GeoTIFF not yet loaded.
    Stub { stub: StacStub },
    /// Fully loaded with GeoTIFF metadata and data source.
    Loaded {
        metadata: Arc<TiffMetadata>,
        source: Arc<DataSource>,
    },
}

/// An entry in the file catalog: one GeoTIFF file with a parsed timestamp.
///
/// `metadata` and `source` are wrapped in `Arc` so that cloning the catalog's
/// `BTreeMap` (required for COW semantics with `ArcSwap`) is cheap — just
/// pointer bumps instead of deep-cloning potentially large `DataSource` buffers.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub file_size: u64,
    pub state: FileState,
}

impl FileEntry {
    /// Create a fully loaded entry (local or remote scan).
    pub fn loaded(
        path: PathBuf,
        source: DataSource,
        metadata: TiffMetadata,
        file_size: u64,
    ) -> Self {
        Self {
            path,
            file_size,
            state: FileState::Loaded {
                metadata: Arc::new(metadata),
                source: Arc::new(source),
            },
        }
    }

    /// Create a stub entry from STAC metadata (no GeoTIFF data loaded yet).
    pub fn stac_stub(path: PathBuf, file_size: u64, stub: StacStub) -> Self {
        Self {
            path,
            file_size,
            state: FileState::Stub { stub },
        }
    }

    /// Whether this entry has full GeoTIFF metadata loaded.
    pub fn is_loaded(&self) -> bool {
        matches!(self.state, FileState::Loaded { .. })
    }

    /// Get metadata if loaded, None if stub.
    pub fn metadata(&self) -> Option<&Arc<TiffMetadata>> {
        match &self.state {
            FileState::Loaded { metadata, .. } => Some(metadata),
            FileState::Stub { .. } => None,
        }
    }

    /// Get source if loaded, None if stub.
    pub fn source(&self) -> Option<&Arc<DataSource>> {
        match &self.state {
            FileState::Loaded { source, .. } => Some(source),
            FileState::Stub { .. } => None,
        }
    }

    /// Get the STAC stub if this is a stub entry.
    pub fn stac_stub_info(&self) -> Option<&StacStub> {
        match &self.state {
            FileState::Stub { stub } => Some(stub),
            FileState::Loaded { .. } => None,
        }
    }

    /// Promote a stub to loaded state with metadata and source.
    pub fn set_loaded(&mut self, metadata: Arc<TiffMetadata>, source: Arc<DataSource>) {
        self.state = FileState::Loaded { metadata, source };
    }

    /// Get mutable access to metadata (for applying overrides). Returns None if stub.
    pub fn metadata_mut(&mut self) -> Option<&mut Arc<TiffMetadata>> {
        match &mut self.state {
            FileState::Loaded { metadata, .. } => Some(metadata),
            FileState::Stub { .. } => None,
        }
    }
}

/// Immutable snapshot of discovered GeoTIFF files, sorted by timestamp.
#[derive(Debug, Clone)]
pub struct Catalog {
    pub entries: BTreeMap<DateTime<Utc>, FileEntry>,
    pub temporal_extent: Option<(DateTime<Utc>, DateTime<Utc>)>,
    pub spatial_extent: Option<[f64; 4]>,
}

impl Catalog {
    pub fn empty() -> Self {
        Catalog {
            entries: BTreeMap::new(),
            temporal_extent: None,
            spatial_extent: None,
        }
    }

    /// Recompute temporal and spatial extents from current entries.
    pub fn recompute_extents(&mut self) {
        self.temporal_extent = match (self.entries.keys().next(), self.entries.keys().next_back()) {
            (Some(&first), Some(&last)) => Some((first, last)),
            _ => None,
        };
        self.spatial_extent = compute_spatial_union(self.entries.values());
    }

    /// Trim to keep only the most recent `max` entries by timestamp.
    pub fn trim_to_latest(&mut self, max: usize) {
        if self.entries.len() <= max {
            return;
        }
        let keep_from = self.entries.keys().rev().nth(max - 1).copied();
        if let Some(cutoff) = keep_from {
            self.entries = self.entries.split_off(&cutoff);
        }
        self.recompute_extents();
    }
}

/// Compute the union bounding box across all file entries.
/// Returns None if there are no entries.
fn compute_spatial_union<'a>(entries: impl Iterator<Item = &'a FileEntry>) -> Option<[f64; 4]> {
    let mut result: Option<[f64; 4]> = None;
    for entry in entries {
        let bbox = match &entry.state {
            FileState::Loaded { metadata, .. } => metadata.geo_transform.bbox(),
            FileState::Stub { stub } => match stub.bbox {
                Some(b) => b,
                None => continue,
            },
        };
        result = Some(match result {
            None => bbox,
            Some([w, s, e, n]) => [
                w.min(bbox[0]),
                s.min(bbox[1]),
                e.max(bbox[2]),
                n.max(bbox[3]),
            ],
        });
    }
    result
}

/// Parse candidate files from an iterator of (filename, file_size) pairs.
///
/// Matches filenames against the regex pattern, extracts timestamps, and
/// returns a vec of (datetime, filename, file_size) tuples.
pub fn parse_candidates_from_names<'a>(
    names: impl Iterator<Item = (&'a str, u64)>,
    pattern: &Regex,
    ts_format: &str,
) -> Vec<(DateTime<Utc>, String, u64)> {
    let mut candidates = Vec::new();
    for (filename, file_size) in names {
        if filename.len() > MAX_FILENAME_LENGTH {
            continue;
        }
        let caps = match pattern.captures(filename) {
            Some(c) => c,
            None => continue,
        };
        let timestamp_str = match caps.name("timestamp") {
            Some(m) => m.as_str(),
            None => continue,
        };
        let datetime = match NaiveDateTime::parse_from_str(timestamp_str, ts_format) {
            Ok(dt) => dt.and_utc(),
            Err(_) => {
                tracing::warn!(
                    "Cannot parse timestamp '{}' from file '{}'",
                    timestamp_str,
                    filename
                );
                continue;
            }
        };
        candidates.push((datetime, filename.to_string(), file_size));
    }
    candidates
}

/// Apply time window filter and max_files limit to a list of candidates.
///
/// Filters by time window (if set), sorts by timestamp descending, truncates
/// to max_files, then re-sorts ascending for BTreeMap insertion order.
pub fn apply_scan_filters(
    candidates: &mut Vec<(DateTime<Utc>, String, u64)>,
    time_filter: Option<(DateTime<Utc>, DateTime<Utc>)>,
    max_files: Option<usize>,
) {
    // Filter by time window
    if let Some((start, end)) = time_filter {
        candidates.retain(|(dt, _, _)| *dt >= start && *dt <= end);
    }

    // Sort by timestamp descending and take only max_files most recent
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    if let Some(max) = max_files {
        candidates.truncate(max);
    }
    // Re-sort ascending for BTreeMap insertion order
    candidates.sort_by_key(|c| c.0);
}

/// Tracks files seen but not yet confirmed as fully written.
#[derive(Debug)]
pub struct PendingFile {
    pub size: u64,
    /// Number of consecutive polls with the same size.
    pub stable_count: u8,
}

/// Scan a directory for GeoTIFF files matching a filename pattern.
///
/// Returns a new Catalog containing all valid files. Files that fail to parse
/// are logged and skipped.
///
/// `existing` provides a path-based index of entries already in the catalog.
/// Files with unchanged size reuse their cached metadata (no re-parse).
pub fn scan_directory(
    dir: &Path,
    pattern: &Regex,
    timestamp_format: &str,
    exclude_patterns: &[String],
    pending: &mut BTreeMap<PathBuf, PendingFile>,
    existing: &HashMap<&Path, &FileEntry>,
) -> Result<Catalog, DataServerError> {
    let read_dir = std::fs::read_dir(dir).map_err(|e| {
        DataServerError::GeoTiff(format!("Cannot read directory {}: {e}", dir.display()))
    })?;

    let mut entries = BTreeMap::new();

    for entry in read_dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let file_name = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue, // non-UTF8 filename
        };

        // Security: skip overly long filenames
        if file_name.len() > MAX_FILENAME_LENGTH {
            continue;
        }

        // Skip excluded patterns
        if is_excluded(&file_name, exclude_patterns) {
            continue;
        }

        // Skip non-files (directories, symlinks)
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if !file_type.is_file() {
            continue;
        }

        // Match filename against pattern
        let caps = match pattern.captures(&file_name) {
            Some(c) => c,
            None => continue,
        };

        let timestamp_str = match caps.name("timestamp") {
            Some(m) => m.as_str().to_string(),
            None => continue,
        };

        // Parse timestamp
        let datetime = match NaiveDateTime::parse_from_str(&timestamp_str, timestamp_format) {
            Ok(dt) => dt.and_utc(),
            Err(_) => {
                tracing::warn!(
                    "Cannot parse timestamp '{}' from file '{}'",
                    timestamp_str,
                    file_name
                );
                continue;
            }
        };

        let path = entry.path();

        // Get file size
        let file_size = match entry.metadata() {
            Ok(m) => m.len(),
            Err(_) => continue,
        };

        // File readiness: check size stability for genuinely NEW files only.
        // Files already in the catalog (existing) skip the readiness check.
        let existing_entry = existing.get(path.as_path());

        if existing_entry.is_none() {
            if let Some(prev) = pending.get_mut(&path) {
                if prev.size != file_size {
                    // Size changed — still being written, reset counter
                    prev.size = file_size;
                    prev.stable_count = 0;
                    continue;
                }
                prev.stable_count += 1;
                if prev.stable_count < 2 {
                    // Need 2 consecutive stable polls before accepting
                    continue;
                }
                // Size stable for 2 polls — promote from pending
                pending.remove(&path);
            } else if !existing.is_empty() {
                // Genuinely new file during a poll cycle — add to pending, skip this cycle
                pending.insert(
                    path.clone(),
                    PendingFile {
                        size: file_size,
                        stable_count: 0,
                    },
                );
                continue;
            }
            // else: initial scan (existing empty) — accept immediately
        }

        // Reuse cached metadata if file size unchanged
        let metadata = if let Some(entry) = existing_entry {
            if entry.file_size == file_size {
                match entry.metadata().map(|m| (**m).clone()) {
                    Some(m) => m,
                    None => match TiffMetadata::from_file(&path) {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::warn!("Skipping {}: {e}", path.display());
                            continue;
                        }
                    },
                }
            } else {
                match TiffMetadata::from_file(&path) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!("Skipping {}: {e}", path.display());
                        continue;
                    }
                }
            }
        } else {
            match TiffMetadata::from_file(&path) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("Skipping {}: {e}", path.display());
                    continue;
                }
            }
        };

        // Handle duplicate timestamps: keep lexicographically last filename
        if let Some(existing) = entries.get(&datetime) {
            let existing_entry: &FileEntry = existing;
            if path.to_string_lossy() <= existing_entry.path.to_string_lossy() {
                continue;
            }
            tracing::warn!(
                "Duplicate timestamp {}: using {}, replacing {}",
                datetime,
                path.display(),
                existing_entry.path.display()
            );
        }

        let source = DataSource::from_path(&path);
        entries.insert(
            datetime,
            FileEntry::loaded(path, source, metadata, file_size),
        );
    }

    // Clean pending files that no longer exist in the directory
    pending.retain(|p, _| p.exists());

    // Compute extents
    let temporal_extent = match (entries.keys().next(), entries.keys().next_back()) {
        (Some(&first), Some(&last)) => Some((first, last)),
        _ => None,
    };

    let spatial_extent = compute_spatial_union(entries.values());

    Ok(Catalog {
        entries,
        temporal_extent,
        spatial_extent,
    })
}

fn is_excluded(filename: &str, patterns: &[String]) -> bool {
    for pattern in patterns {
        if pattern.starts_with("*.") {
            // Extension match
            let ext = &pattern[1..]; // e.g. ".tmp"
            if filename.ends_with(ext) {
                return true;
            }
        } else if pattern.starts_with('.') {
            // Hidden file match
            if filename.starts_with('.') {
                return true;
            }
        } else if filename == pattern {
            return true;
        }
    }
    false
}

/// Maximum file size for remote downloads (50 MB).
pub(crate) const MAX_REMOTE_FILE_SIZE: usize = 50 * 1024 * 1024;

/// Scan a remote object store for GeoTIFF files matching a filename pattern.
///
/// Uses COG-style byte-range reads to fetch only the IFD metadata (first 64 KB)
/// instead of downloading entire files. Falls back to full download if the
/// header-only parse fails (e.g., non-COG layout or unsupported compression).
///
/// `existing` provides a path-based index of entries already in the catalog.
/// Files with unchanged size reuse their cached entry (no re-download).
#[allow(clippy::too_many_arguments)]
pub fn scan_remote_with_limit(
    store: &ds_storage::DataStore,
    prefix: &ds_storage::object_store::path::Path,
    pattern: &Regex,
    timestamp_format: &str,
    existing: &HashMap<&Path, &FileEntry>,
    max_files: Option<usize>,
    time_filter: Option<(DateTime<Utc>, DateTime<Utc>)>,
    collection_id: &str,
) -> Result<Catalog, DataServerError> {
    let entries_list = store.list(prefix)?;

    // Build location index and extract basenames for pattern matching.
    // parse_candidates_from_names works with basenames; we keep the full key
    // in a parallel vec so we can look up the object_store path afterward.
    let remote_entries: Vec<(String, String, u64, ds_storage::object_store::path::Path)> =
        entries_list
            .iter()
            .filter_map(|obj| {
                if obj.size > MAX_REMOTE_FILE_SIZE {
                    return None;
                }
                let key = obj.location.to_string();
                let filename = key.rsplit('/').next().unwrap_or(&key).to_string();
                Some((key, filename, obj.size as u64, obj.location.clone()))
            })
            .collect();

    let mut candidates = parse_candidates_from_names(
        remote_entries
            .iter()
            .map(|(_, filename, size, _)| (filename.as_str(), *size)),
        pattern,
        timestamp_format,
    );

    apply_scan_filters(&mut candidates, time_filter, max_files);

    // Build a filename→(key, location) lookup for resolving full paths.
    // (candidates contain basenames from parse_candidates_from_names)
    let filename_to_remote: HashMap<&str, (&str, &ds_storage::object_store::path::Path)> =
        remote_entries
            .iter()
            .map(|(key, filename, _, location)| (filename.as_str(), (key.as_str(), location)))
            .collect();

    let listed = entries_list.len();
    let kept = candidates.len();
    if time_filter.is_some() {
        tracing::info!(
            "[{}] Prefix '{}': {} listed, {} within time window",
            collection_id,
            prefix,
            listed,
            kept
        );
    } else {
        tracing::info!(
            "[{}] Prefix '{}': {} listed, {} matching",
            collection_id,
            prefix,
            listed,
            kept
        );
    }

    // Second pass: parse metadata (range read, falling back to full download)
    let mut entries = BTreeMap::new();

    for (datetime, filename, file_size) in &candidates {
        let &(key, location) = match filename_to_remote.get(filename.as_str()) {
            Some(kl) => kl,
            None => continue, // shouldn't happen
        };
        let pseudo_path = PathBuf::from(key);

        // Reuse cached entry if file size unchanged
        if let Some(entry) = existing.get(pseudo_path.as_path()) {
            if entry.file_size == *file_size {
                entries.insert(*datetime, (*entry).clone());
                continue;
            }
        }

        // Try COG range read first (header only)
        if let Some((metadata, tile_info)) =
            TiffMetadata::from_header_read(store, location, *file_size)
        {
            tracing::debug!("[{}] {} — range read OK", collection_id, key);
            let source = DataSource::Remote {
                store: store.clone(),
                path: location.clone(),
                tile_info,
            };
            entries.insert(
                *datetime,
                FileEntry::loaded(pseudo_path, source, metadata, *file_size),
            );
            continue;
        }

        // Fallback: download full file
        tracing::info!(
            "[{}] {} — range read failed, downloading full file ({})",
            collection_id,
            key,
            super::format_bytes(*file_size)
        );
        let data = match store.get(location) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("[{}] Failed to download {}: {e}", collection_id, key);
                continue;
            }
        };

        let source = DataSource::from_bytes(data);
        let metadata = match TiffMetadata::from_source(&source) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("[{}] Skipping {}: {e}", collection_id, key);
                continue;
            }
        };

        entries.insert(
            *datetime,
            FileEntry::loaded(pseudo_path, source, metadata, *file_size),
        );
    }

    let temporal_extent = match (entries.keys().next(), entries.keys().next_back()) {
        (Some(&first), Some(&last)) => Some((first, last)),
        _ => None,
    };

    let spatial_extent = compute_spatial_union(entries.values());

    Ok(Catalog {
        entries,
        temporal_extent,
        spatial_extent,
    })
}

/// Create a catalog seeded from STAC collection extent — no items fetched.
///
/// This is called at engine startup. The catalog has no entries but carries
/// the spatial and temporal extent from the STAC collection metadata.
/// Items are fetched on-demand when queries arrive.
pub fn init_stac_from_extent(extent: &crate::stac::StacExtent) -> Catalog {
    let temporal_extent = extent.temporal_start.map(|start| {
        let end = extent.temporal_end.unwrap_or_else(Utc::now);
        (start, end)
    });

    Catalog {
        entries: BTreeMap::new(),
        temporal_extent,
        spatial_extent: extent.spatial_bbox,
    }
}

/// Fetch STAC items for a specific datetime range and merge into the catalog.
///
/// Called on-demand from query path when entries are needed for a time range.
/// Creates lightweight stub entries — no GeoTIFF downloads.
/// Existing entries (both stubs and loaded) are preserved.
pub fn fetch_stac_range(
    client: &crate::stac::StacClient,
    existing_catalog: &Catalog,
    time_range: (chrono::DateTime<Utc>, chrono::DateTime<Utc>),
    collection_id: &str,
) -> Result<Catalog, DataServerError> {
    let items = client.fetch_items(Some(time_range), None)?;

    tracing::info!(
        "[{}] STAC: fetched {} items for range {}/{}",
        collection_id,
        items.len(),
        time_range.0.format("%Y-%m-%dT%H:%MZ"),
        time_range.1.format("%Y-%m-%dT%H:%MZ"),
    );

    // Start with existing entries
    let mut entries = existing_catalog.entries.clone();

    for item in &items {
        // Skip if we already have this timestamp
        if entries.contains_key(&item.datetime) {
            continue;
        }

        let pseudo_path = PathBuf::from(&item.asset_url);
        let file_size = item.file_size.unwrap_or(0);

        entries.insert(
            item.datetime,
            FileEntry::stac_stub(
                pseudo_path,
                file_size,
                StacStub {
                    bbox: item.bbox,
                    asset_url: item.asset_url.clone(),
                },
            ),
        );
    }

    let temporal_extent = existing_catalog.temporal_extent;
    let spatial_extent = existing_catalog.spatial_extent;

    Ok(Catalog {
        entries,
        temporal_extent,
        spatial_extent,
    })
}

/// Poll STAC for new items since the latest catalog entry.
///
/// Used by the poll loop to pick up newly published data.
/// Only fetches items newer than the most recent existing entry.
pub fn poll_stac_latest(
    client: &crate::stac::StacClient,
    existing_catalog: &Catalog,
    collection_id: &str,
) -> Result<Catalog, DataServerError> {
    // Refresh extent (temporal end may have advanced)
    let extent = client.fetch_extent().unwrap_or_else(|e| {
        tracing::warn!("[{}] STAC extent refresh failed: {}", collection_id, e);
        crate::stac::StacExtent {
            spatial_bbox: existing_catalog.spatial_extent,
            temporal_start: existing_catalog.temporal_extent.map(|(s, _)| s),
            temporal_end: None,
        }
    });

    // Fetch items newer than our latest entry
    let since = existing_catalog
        .entries
        .keys()
        .next_back()
        .map(|latest| *latest + chrono::Duration::seconds(1))
        .unwrap_or_else(|| Utc::now() - chrono::Duration::hours(1));

    let items = client.fetch_items(Some((since, Utc::now())), None)?;

    let new_count = items.len();
    if new_count > 0 {
        tracing::info!(
            "[{}] STAC poll: {} new items (total: {})",
            collection_id,
            new_count,
            existing_catalog.entries.len() + new_count
        );
    }

    let mut entries = existing_catalog.entries.clone();

    for item in &items {
        if entries.contains_key(&item.datetime) {
            continue;
        }
        let pseudo_path = PathBuf::from(&item.asset_url);
        let file_size = item.file_size.unwrap_or(0);
        entries.insert(
            item.datetime,
            FileEntry::stac_stub(
                pseudo_path,
                file_size,
                StacStub {
                    bbox: item.bbox,
                    asset_url: item.asset_url.clone(),
                },
            ),
        );
    }

    // Update temporal extent from collection metadata
    let temporal_extent = extent.temporal_start.map(|start| {
        let end = extent.temporal_end.unwrap_or_else(Utc::now);
        (start, end)
    });

    Ok(Catalog {
        entries,
        temporal_extent,
        spatial_extent: extent.spatial_bbox.or(existing_catalog.spatial_extent),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::{DataSource, TiffMetadata};
    use ds_core::geo::{Crs, GeoTransform};

    #[test]
    fn exclude_patterns() {
        assert!(is_excluded("data.tmp", &["*.tmp".into()]));
        assert!(is_excluded("data.part", &["*.part".into()]));
        assert!(is_excluded(".hidden", &[".*".into()]));
        assert!(!is_excluded(
            "radar_20240101T0000Z.tif",
            &["*.tmp".into(), "*.part".into()]
        ));
    }

    fn dummy_metadata() -> TiffMetadata {
        TiffMetadata {
            width: 100,
            height: 100,
            tile_width: 256,
            tile_height: 256,
            tiles_across: 1,
            tiles_down: 1,
            samples_per_pixel: 1,
            geo_transform: GeoTransform {
                origin_x: 0.0,
                origin_y: 0.0,
                pixel_width: 0.01,
                pixel_height: -0.01,
                width: 100,
                height: 100,
                crs: Crs::Wgs84,
            },
            nodata: None,
            scale: None,
            offset: None,
            overviews: vec![],
        }
    }

    #[test]
    fn file_entry_loaded_constructor() {
        let path = PathBuf::from("/tmp/test.tif");
        let source = DataSource::from_path(&path);
        let metadata = dummy_metadata();
        let entry = FileEntry::loaded(path.clone(), source, metadata, 1024);

        assert!(entry.metadata().is_some());
        assert!(entry.source().is_some());
        assert!(entry.stac_stub_info().is_none());
        assert_eq!(entry.file_size, 1024);
        assert_eq!(entry.path, path);
    }

    #[test]
    fn file_entry_stac_stub_constructor() {
        let path = PathBuf::from("https://example.com/radar/file.tif");
        let stub = StacStub {
            bbox: Some([10.0, 55.0, 30.0, 72.0]),
            asset_url: "https://example.com/radar/file.tif".to_string(),
        };
        let entry = FileEntry::stac_stub(path.clone(), 2048, stub);

        assert!(entry.metadata().is_none());
        assert!(entry.source().is_none());
        assert!(entry.stac_stub_info().is_some());
        assert_eq!(entry.file_size, 2048);
        assert_eq!(entry.path, path);
        let stub = entry.stac_stub_info().unwrap();
        assert_eq!(stub.bbox, Some([10.0, 55.0, 30.0, 72.0]));
        assert_eq!(stub.asset_url, "https://example.com/radar/file.tif");
    }

    #[test]
    fn file_entry_is_loaded() {
        // A loaded entry should return true
        let path = PathBuf::from("/tmp/test.tif");
        let source = DataSource::from_path(&path);
        let metadata = dummy_metadata();
        let loaded = FileEntry::loaded(path, source, metadata, 1024);
        assert!(loaded.is_loaded());

        // A stub entry should return false
        let stub_path = PathBuf::from("https://example.com/file.tif");
        let stub = StacStub {
            bbox: None,
            asset_url: "https://example.com/file.tif".to_string(),
        };
        let unloaded = FileEntry::stac_stub(stub_path, 0, stub);
        assert!(!unloaded.is_loaded());
    }
}
