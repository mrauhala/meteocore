//! Data sources. `Local`: a directory / object-store prefix of BUFR files,
//! re-listed every poll and fetched incrementally. (The WIS2 push source
//! lives in `wis2.rs` — not in this PR.)
//!
//! All I/O here goes through `ds-storage`, a sync bridge that is only valid
//! on the background poll runtime (Critical Rule 7) — `scan()` is called
//! from `poll_loop` only.

use std::collections::HashMap;

use ds_core::error::DataServerError;
use ds_storage::object_store::path::Path as ObjectPath;
use ds_storage::{build_store, DataStore};

/// Concurrent object fetches per scan (Rule 9: never a sequential loop).
const FETCH_CONCURRENCY: usize = 8;
/// Largest BUFR file fetched (a SYNOP bulletin is tens of KiB; a whole
/// TEMP collective a few MiB).
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// Files listed per scan beyond which the scan is truncated (with a WARN).
const MAX_LISTED: usize = 50_000;
/// Remembered `(path, size, mtime)` keys; oldest forgotten beyond this.
const MAX_SEEN: usize = 200_000;

const EXTENSIONS: &[&str] = &["bufr", "bufr4", "bin", "b", "bfr"];

pub struct LocalSource {
    store: DataStore,
    base: ObjectPath,
    label: String,
    /// path → (size, last_modified millis) of files already ingested.
    seen: HashMap<String, (u64, i64)>,
    seen_order: std::collections::VecDeque<String>,
}

/// One fetched file.
pub struct Fetched {
    pub path: String,
    pub bytes: ds_storage::bytes::Bytes,
}

impl LocalSource {
    pub fn new(data_path: &str) -> Result<Self, DataServerError> {
        let (store, base) = build_store(data_path)?;
        Ok(LocalSource {
            store,
            base,
            label: data_path.to_string(),
            seen: HashMap::new(),
            seen_order: std::collections::VecDeque::new(),
        })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// List the prefix and fetch every BUFR file not seen before (or whose
    /// size / mtime changed). Blocking; poll runtime only.
    pub fn scan(&mut self) -> Result<Vec<Fetched>, DataServerError> {
        let mut listed = self.store.list(&self.base)?;
        if listed.len() > MAX_LISTED {
            tracing::warn!(
                "bufr: '{}' lists {} objects — only the first {MAX_LISTED} are considered",
                self.label,
                listed.len()
            );
            listed.truncate(MAX_LISTED);
        }
        let mut wanted: Vec<(ObjectPath, u64, i64)> = Vec::new();
        for m in listed {
            let name = m.location.as_ref();
            let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
            if !EXTENSIONS.contains(&ext.as_str()) {
                continue;
            }
            if m.size > MAX_FILE_BYTES {
                tracing::warn!("bufr: skipping '{name}' ({} bytes > cap)", m.size);
                continue;
            }
            let key = (m.size, m.last_modified.timestamp_millis());
            if self.seen.get(name) == Some(&key) {
                continue;
            }
            wanted.push((m.location.clone(), key.0, key.1));
        }
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let paths: Vec<ObjectPath> = wanted.iter().map(|w| w.0.clone()).collect();
        let results = self
            .store
            .get_many(&paths, FETCH_CONCURRENCY, Some(MAX_FILE_BYTES))?;
        let mut out = Vec::with_capacity(paths.len());
        for ((path, size, mtime), res) in wanted.into_iter().zip(results) {
            match res {
                Ok(bytes) => {
                    self.remember(path.as_ref(), (size, mtime));
                    out.push(Fetched {
                        path: path.to_string(),
                        bytes,
                    });
                }
                Err(e) => tracing::warn!("bufr: fetch of '{path}' failed: {e}"),
            }
        }
        Ok(out)
    }

    fn remember(&mut self, path: &str, key: (u64, i64)) {
        if self.seen.insert(path.to_string(), key).is_none() {
            self.seen_order.push_back(path.to_string());
            while self.seen_order.len() > MAX_SEEN {
                if let Some(old) = self.seen_order.pop_front() {
                    self.seen.remove(&old);
                }
            }
        }
    }
}
