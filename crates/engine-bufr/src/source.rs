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
/// Files per `get_many` call. `get_many` buffers a whole batch's bytes
/// before returning, so this — not the listing size — bounds both the
/// length of one blocking call and peak memory (≤ `FETCH_CHUNK` ×
/// `MAX_FILE_BYTES`); each chunk is handed to the sink and dropped before
/// the next is fetched (the engine-odim convention).
const FETCH_CHUNK: usize = FETCH_CONCURRENCY;
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
    /// size / mtime changed), handing each file to `sink` chunk by chunk so
    /// a backlog never sits in memory whole. Returns the number of files
    /// fetched. Blocking; poll runtime only. `Err` when the listing fails
    /// or when every fetch failed (nothing reached the sink).
    pub fn scan(&mut self, mut sink: impl FnMut(Fetched)) -> Result<usize, DataServerError> {
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
            return Ok(0);
        }
        let mut fetched = 0usize;
        let mut failed_chunks = 0usize;
        for chunk in wanted.chunks(FETCH_CHUNK) {
            let paths: Vec<ObjectPath> = chunk.iter().map(|w| w.0.clone()).collect();
            let results = match self
                .store
                .get_many(&paths, FETCH_CONCURRENCY, Some(MAX_FILE_BYTES))
            {
                Ok(r) => r,
                Err(e) => {
                    failed_chunks += 1;
                    tracing::warn!("bufr: batch fetch under '{}' failed: {e}", self.label);
                    continue;
                }
            };
            for ((path, size, mtime), res) in chunk.iter().zip(results) {
                match res {
                    Ok(bytes) => {
                        self.remember(path.as_ref(), (*size, *mtime));
                        fetched += 1;
                        sink(Fetched {
                            path: path.to_string(),
                            bytes,
                        });
                    }
                    Err(e) => tracing::warn!("bufr: fetch of '{path}' failed: {e}"),
                }
            }
        }
        if fetched == 0 && failed_chunks > 0 {
            return Err(DataServerError::Storage(format!(
                "every batch fetch under '{}' failed",
                self.label
            )));
        }
        Ok(fetched)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/bufr-synop")
            .join(name);
        std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }

    #[test]
    fn scan_streams_every_new_file_in_bounded_chunks_and_remembers_them() {
        let dir = tempfile::tempdir().unwrap();
        let smhi = fixture("synop_se-smhi_20260912T0800Z.bufr");
        // More files than one fetch chunk, plus a non-BUFR extension that
        // must be ignored.
        let n = FETCH_CHUNK * 3 + 1;
        for i in 0..n {
            std::fs::write(dir.path().join(format!("m{i:03}.bufr")), &smhi).unwrap();
        }
        std::fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        let mut src = LocalSource::new(dir.path().to_str().unwrap()).unwrap();

        let mut got: Vec<String> = Vec::new();
        let fetched = src
            .scan(|f| {
                assert_eq!(f.bytes.len(), smhi.len());
                got.push(f.path);
            })
            .unwrap();
        assert_eq!(fetched, n);
        assert_eq!(got.len(), n);
        assert!(got.iter().all(|p| p.ends_with(".bufr")));

        // Nothing new: the sink is never called.
        let again = src
            .scan(|_| panic!("unchanged files must not be refetched"))
            .unwrap();
        assert_eq!(again, 0);

        // A rewritten file (different size) is fetched again, alone.
        let mut changed = smhi.clone();
        changed.extend_from_slice(b"7777");
        std::fs::write(dir.path().join("m000.bufr"), &changed).unwrap();
        let mut refetched = Vec::new();
        let n2 = src.scan(|f| refetched.push(f.path)).unwrap();
        assert_eq!(n2, 1);
        assert!(refetched[0].ends_with("m000.bufr"));
    }
}
