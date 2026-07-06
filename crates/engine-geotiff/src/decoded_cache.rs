//! Process-global cache of **decoded** GeoTIFF chunks for local sources (#463).
//!
//! The WMS meta-tile loop renders a full viewport as ~50–190 independent
//! `get_raster_tile` calls, and adjacent meta-tiles share covering source
//! tiles — without a decode memo each 512×512 source tile is
//! LZW/DEFLATE-decoded ~6× per frame. The compressed-byte [`crate::cache::TileCache`]
//! deliberately doesn't help here: for a local mmap'd file the compressed
//! bytes are already free from the page cache; the redundant cost is
//! decompress + predictor. So this cache memoizes the *decoded* chunk —
//! the native [`DecodingResult`] buffer (262 KB for a 512×512 Byte tile),
//! with band extraction / nodata mapping / scale-offset deferred to the
//! copy into the output window.
//!
//! Keying: `(path, mtime, size, inode, ifd, chunk)`. The identity is
//! captured from the same file handle the mmap was created from, so a file
//! replaced via atomic rename (new inode → new identity on the next catalog
//! scan) can never serve stale pixels; stale-generation entries age out of
//! the LRU. Same single-flight + byte-bounded LRU shape as `engine-odim`'s
//! `COMPOSITE_CACHE` (#212).
//!
//! `MC_GEOTIFF_DECODED_CHUNK_CACHE_MB` sizes the cache (default 512 MB);
//! `0` disables retention (`capacity.max(1)` keeps the cache valid but
//! unable to hold anything, so every read decodes — same convention as the
//! ODIM caches).

use std::sync::Arc;

use ds_core::error::DataServerError;
use tiff::decoder::DecodingResult;

/// Identity of a local file's contents, captured from the open file handle
/// at mmap time (so it describes exactly the bytes the decoder sees, not
/// whatever inode currently sits at the path).
///
/// Carries `inode` alongside `mtime` + `size` for the same reason the
/// catalog's unchanged-file test does (#253 rounds 2–3): size alone misses a
/// same-byte-count atomic replacement, and mtime alone misses the same-second
/// case on 1-second-resolution filesystems because `rename(2)` doesn't bump
/// the renamed file's mtime. Every atomic rename produces a new inode, so
/// including it keeps a same-size same-second replacement from colliding
/// with the old file's cached chunks. On non-Unix (no `MetadataExt::ino()`)
/// it stays 0 — mtime+size only, same fallback the catalog uses.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct FileIdentity {
    pub mtime_ns: u64,
    pub size: u64,
    pub inode: u64,
}

impl FileIdentity {
    pub(crate) fn from_metadata(meta: &std::fs::Metadata) -> Self {
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            meta.ino()
        };
        #[cfg(not(unix))]
        let inode = 0u64;
        FileIdentity {
            mtime_ns,
            size: meta.len(),
            inode,
        }
    }
}

/// Cache scope for one local file: its path plus the content identity the
/// mmap was captured under. Built once per `read_bbox*` call, cheap to clone.
#[derive(Clone)]
pub(crate) struct FileScope {
    file: Arc<str>,
    identity: FileIdentity,
}

impl FileScope {
    pub(crate) fn new(file: impl Into<Arc<str>>, identity: FileIdentity) -> Self {
        FileScope {
            file: file.into(),
            identity,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ChunkKey {
    file: Arc<str>,
    identity: FileIdentity,
    /// IFD index (0 = full resolution, 1+ = overview levels).
    ifd_index: u16,
    chunk_index: u32,
}

/// Number of samples (all bands interleaved) in a decoded chunk.
pub(crate) fn sample_count(chunk: &DecodingResult) -> usize {
    match chunk {
        DecodingResult::U8(v) => v.len(),
        DecodingResult::I8(v) => v.len(),
        DecodingResult::U16(v) => v.len(),
        DecodingResult::I16(v) => v.len(),
        DecodingResult::F16(v) => v.len(),
        DecodingResult::U32(v) => v.len(),
        DecodingResult::I32(v) => v.len(),
        DecodingResult::F32(v) => v.len(),
        DecodingResult::U64(v) => v.len(),
        DecodingResult::I64(v) => v.len(),
        DecodingResult::F64(v) => v.len(),
    }
}

/// Payload bytes of a decoded chunk (for LRU weighting).
fn byte_len(chunk: &DecodingResult) -> u64 {
    let elem: u64 = match chunk {
        DecodingResult::U8(_) | DecodingResult::I8(_) => 1,
        DecodingResult::U16(_) | DecodingResult::I16(_) | DecodingResult::F16(_) => 2,
        DecodingResult::U32(_) | DecodingResult::I32(_) | DecodingResult::F32(_) => 4,
        DecodingResult::U64(_) | DecodingResult::I64(_) | DecodingResult::F64(_) => 8,
    };
    sample_count(chunk) as u64 * elem
}

/// Byte-weights each cached chunk by its decoded buffer (plus key string and
/// `Arc`/control overhead). Mirrors the compressed `TileCache` weighter.
fn weigh_chunk(key: &ChunkKey, val: &Arc<DecodingResult>) -> u64 {
    byte_len(val) + key.file.len() as u64 + 64
}

/// Default cache size (MB) when `MC_GEOTIFF_DECODED_CHUNK_CACHE_MB` is unset.
/// A 512×512 Byte tile decodes to 262 KB, so 512 MB holds ~2000 tiles — a
/// 13-frame full-viewport animation of the FMI 5120×6144 composite (~72
/// covering tiles/frame ≈ 245 MB) fits with headroom, and a Float32 source
/// (1 MB/tile) still keeps ~500 tiles resident.
const DEFAULT_DECODED_CHUNK_CACHE_MB: u64 = 512;

type ChunkCache = ds_cache::ByteBoundedCache<ChunkKey, Arc<DecodingResult>>;

static CACHE: std::sync::LazyLock<ChunkCache> = std::sync::LazyLock::new(|| {
    // Estimate item slots at one 512×512 Byte tile (256 KB) each.
    ChunkCache::from_env(
        "MC_GEOTIFF_DECODED_CHUNK_CACHE_MB",
        DEFAULT_DECODED_CHUNK_CACHE_MB,
        256 * 1024,
        weigh_chunk,
    )
});

/// Snapshot of the process-global decoded-chunk cache for `/metrics`.
pub fn metrics() -> ds_cache::CacheMetrics {
    CACHE.metrics()
}

/// Fetch the decoded chunk for `(scope, ifd, chunk)` from the cache, or run
/// `decode` and insert. `get_or_insert_with` is the single-flight: concurrent
/// callers for the SAME chunk block on one decode. A decode error is returned
/// to this caller without inserting (no key poisoning); the miss is counted
/// before the fallible decode so failures still register as misses.
pub(crate) fn get_or_decode(
    scope: &FileScope,
    ifd_index: u16,
    chunk_index: u32,
    decode: impl FnOnce() -> Result<DecodingResult, DataServerError>,
) -> Result<Arc<DecodingResult>, DataServerError> {
    let key = ChunkKey {
        file: Arc::clone(&scope.file),
        identity: scope.identity,
        ifd_index,
        chunk_index,
    };
    CACHE.get_or_insert_with(&key, || decode().map(Arc::new))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A changed file identity (mtime/size/inode) must be a distinct cache
    /// entry — this is the stale-pixel guard for files replaced via atomic
    /// rename.
    #[test]
    fn changed_identity_is_a_fresh_entry() {
        let id_a = FileIdentity {
            mtime_ns: 1,
            size: 100,
            inode: 7,
        };
        let id_b = FileIdentity {
            mtime_ns: 2,
            size: 100,
            inode: 7,
        };
        let scope_a = FileScope::new("decoded_cache_test/file.tif", id_a);
        let scope_b = FileScope::new("decoded_cache_test/file.tif", id_b);

        let a = get_or_decode(&scope_a, 0, 7, || Ok(DecodingResult::U8(vec![1]))).unwrap();
        // Same identity: served from cache, decode closure must NOT run.
        let a2 =
            get_or_decode(&scope_a, 0, 7, || panic!("cached entry must not re-decode")).unwrap();
        // New identity at the same path: must re-decode, not serve stale.
        let b = get_or_decode(&scope_b, 0, 7, || Ok(DecodingResult::U8(vec![2]))).unwrap();

        assert!(matches!(*a, DecodingResult::U8(ref v) if v == &[1]));
        assert!(matches!(*a2, DecodingResult::U8(ref v) if v == &[1]));
        assert!(matches!(*b, DecodingResult::U8(ref v) if v == &[2]));
    }

    /// The #253 round-3 case: an atomic rename can leave mtime AND size
    /// identical (rename(2) doesn't bump the renamed file's mtime; the new
    /// file can be the same byte count) — only the inode changes. The cache
    /// must treat that as a fresh file, not serve the replaced file's chunks.
    #[test]
    fn same_mtime_same_size_different_inode_is_a_fresh_entry() {
        let id_old = FileIdentity {
            mtime_ns: 5,
            size: 400,
            inode: 100,
        };
        let id_new = FileIdentity {
            mtime_ns: 5,
            size: 400,
            inode: 101,
        };
        let scope_old = FileScope::new("decoded_cache_test/renamed.tif", id_old);
        let scope_new = FileScope::new("decoded_cache_test/renamed.tif", id_new);

        let old = get_or_decode(&scope_old, 0, 3, || Ok(DecodingResult::U8(vec![1]))).unwrap();
        let new = get_or_decode(&scope_new, 0, 3, || Ok(DecodingResult::U8(vec![2]))).unwrap();
        assert!(matches!(*old, DecodingResult::U8(ref v) if v == &[1]));
        assert!(
            matches!(*new, DecodingResult::U8(ref v) if v == &[2]),
            "same-mtime same-size rename must not serve the old file's chunks"
        );
    }

    /// IFD index must separate full-res from overview chunks with the same
    /// chunk index (same collision the compressed TileCache guards against).
    #[test]
    fn ifd_index_separates_levels() {
        let scope = FileScope::new(
            "decoded_cache_test/levels.tif",
            FileIdentity {
                mtime_ns: 3,
                size: 200,
                inode: 8,
            },
        );
        let full = get_or_decode(&scope, 0, 0, || Ok(DecodingResult::U8(vec![10]))).unwrap();
        let over = get_or_decode(&scope, 1, 0, || Ok(DecodingResult::U8(vec![20]))).unwrap();
        assert!(matches!(*full, DecodingResult::U8(ref v) if v == &[10]));
        assert!(matches!(*over, DecodingResult::U8(ref v) if v == &[20]));
    }

    /// A decode error must not poison the key: the next attempt re-decodes.
    #[test]
    fn decode_error_does_not_poison() {
        let scope = FileScope::new(
            "decoded_cache_test/err.tif",
            FileIdentity {
                mtime_ns: 4,
                size: 300,
                inode: 9,
            },
        );
        let err = get_or_decode(&scope, 0, 1, || Err(DataServerError::Engine("boom".into())));
        assert!(err.is_err());
        let ok = get_or_decode(&scope, 0, 1, || Ok(DecodingResult::U8(vec![42]))).unwrap();
        assert!(matches!(*ok, DecodingResult::U8(ref v) if v == &[42]));
    }
}
