use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDateTime, Utc};
use ds_core::error::DataServerError;
use regex::Regex;

use crate::reader::{DataSource, TiffMetadata};

/// Maximum filename length to prevent abuse.
const MAX_FILENAME_LENGTH: usize = 255;

/// An entry in the file catalog: one GeoTIFF file with a parsed timestamp.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub source: DataSource,
    pub metadata: TiffMetadata,
    pub file_size: u64,
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
        let bbox = entry.metadata.geo_transform.bbox();
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
                entry.metadata.clone()
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
            FileEntry {
                path,
                source,
                metadata,
                file_size,
            },
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
const MAX_REMOTE_FILE_SIZE: usize = 50 * 1024 * 1024;

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

    // First pass: match filenames and parse timestamps without downloading
    let mut candidates: Vec<(
        DateTime<Utc>,
        String,
        u64,
        ds_storage::object_store::path::Path,
    )> = Vec::new();

    for obj in &entries_list {
        let key = obj.location.to_string();
        let filename = key.rsplit('/').next().unwrap_or(&key);

        if filename.len() > MAX_FILENAME_LENGTH {
            continue;
        }

        let caps = match pattern.captures(filename) {
            Some(c) => c,
            None => continue,
        };

        let timestamp_str = match caps.name("timestamp") {
            Some(m) => m.as_str().to_string(),
            None => continue,
        };

        let datetime = match NaiveDateTime::parse_from_str(&timestamp_str, timestamp_format) {
            Ok(dt) => dt.and_utc(),
            Err(_) => continue,
        };

        // Filter by time window before downloading anything
        if let Some((start, end)) = time_filter {
            if datetime < start || datetime > end {
                continue;
            }
        }

        if obj.size > MAX_REMOTE_FILE_SIZE {
            continue;
        }

        candidates.push((datetime, key, obj.size as u64, obj.location.clone()));
    }

    // Sort by timestamp descending and take only max_files most recent
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    if let Some(max) = max_files {
        candidates.truncate(max);
    }
    // Re-sort ascending for the BTreeMap
    candidates.sort_by_key(|c| c.0);

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

    for (datetime, key, file_size, location) in &candidates {
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
                FileEntry {
                    path: pseudo_path,
                    source,
                    metadata,
                    file_size: *file_size,
                },
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
            FileEntry {
                path: pseudo_path,
                source,
                metadata,
                file_size: *file_size,
            },
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
