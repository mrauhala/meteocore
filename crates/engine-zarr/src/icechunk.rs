//! Icechunk source for the Zarr engine (feature `icechunk`, issue #335).
//!
//! Opens an Icechunk repository (transactional/versioned Zarr) read-only at a
//! chosen version and exposes it through `zarrs_icechunk::AsyncIcechunkStore`.
//! Each storage operation, including consuming range streams, observes the
//! request's absolute deadline on a persistent I/O runtime.
//!
//! Icechunk owns its own object storage (S3/local), so this path does **not**
//! go through `ds-storage` (a deliberate deviation — Icechunk is the storage
//! engine).

use std::collections::HashMap;
use std::sync::Arc;

use futures::TryStreamExt;
use icechunk::repository::VersionInfo;
use icechunk::{Repository, RepositoryConfig};
use zarrs::storage::{
    byte_range::ByteRangeIterator, AsyncListableStorageTraits, AsyncReadableStorageTraits,
    ListableStorageTraits, MaybeBytes, MaybeBytesIterator, ReadableStorageTraits, StorageError,
    StoreKey, StoreKeys, StoreKeysPrefixes, StorePrefix,
};
use zarrs_icechunk::AsyncIcechunkStore;

use ds_core::config::{IcechunkConfig, ZarrConfig};
use ds_core::error::DataServerError;

use crate::{
    runtime,
    store::{io_err, EngineStore},
};

/// Repository clients and immutable object caches survive snapshot refreshes.
pub(crate) struct Source {
    repo: Repository,
    version: VersionInfo,
}

impl Source {
    /// Open repository clients once, with the configured payload cache budget.
    pub(crate) fn open(collection_id: &str, config: &ZarrConfig) -> Result<Self, DataServerError> {
        let ic = config
            .icechunk
            .as_ref()
            .expect("Icechunk source opened without [zarr.icechunk]");

        let cfg_err =
            |msg: String| DataServerError::Config(format!("Collection '{collection_id}': {msg}"));

        let version = version_info(collection_id, ic)?;
        let repo = runtime::run(async {
            let storage = build_storage(collection_id, config).await?;
            // Repository::open merges overrides into the persisted config,
            // including a field-by-field CachingConfig::merge. These derived
            // Defaults leave fields as None (unset), so only num_bytes_chunks
            // changes; metadata caches, compression, storage and virtual chunk
            // containers survive. Covered with non-default V1/V2 repositories.
            let options = RepositoryConfig {
                caching: Some(icechunk::config::CachingConfig {
                    num_bytes_chunks: Some(config.cache_mb.saturating_mul(ds_cache::MIB)),
                    ..Default::default()
                }),
                ..Default::default()
            };
            Repository::open(Some(options), storage, HashMap::new())
                .await
                .map_err(|e| cfg_err(format!("failed to open Icechunk repository: {e}")))
        })?;
        Ok(Self { repo, version })
    }

    pub(crate) fn snapshot(
        &self,
        published: Option<&str>,
    ) -> Result<Option<Arc<EngineStore>>, DataServerError> {
        runtime::run(async {
            let snapshot = self
                .repo
                .resolve_version(&self.version)
                .await
                .map_err(|e| DataServerError::Storage(format!("resolve Icechunk version: {e}")))?;
            let revision = snapshot.to_string();
            if published == Some(revision.as_str()) {
                return Ok(None);
            }
            // Pin the resolved ID so a concurrent commit cannot change the session
            // between checking its revision and opening it.
            let session = self
                .repo
                .readonly_session(&VersionInfo::SnapshotId(snapshot))
                .await
                .map_err(|e| DataServerError::Storage(format!("open Icechunk snapshot: {e}")))?;
            Ok(Some(Arc::new(
                EngineStore::new(Store(AsyncIcechunkStore::new(session))).with_revision(revision),
            )))
        })
    }
}

/// Unlike AsyncToSyncBlockOn (whose output type is unconstrained), this bridge
/// can return a timeout error without panicking or leaving a running I/O task.
struct Store(AsyncIcechunkStore);

fn storage_call<F, T>(future: F) -> Result<T, StorageError>
where
    F: std::future::Future<Output = Result<T, StorageError>> + Send,
    T: Send,
{
    runtime::run(async {
        future
            .await
            .map_err(|e| DataServerError::Storage(e.to_string()))
    })
    .map_err(io_err)
}

impl ReadableStorageTraits for Store {
    fn get(&self, key: &StoreKey) -> Result<MaybeBytes, StorageError> {
        let Some(encoded) = crate::encoded::current() else {
            return storage_call(self.0.get(key));
        };
        runtime::run(async {
            // Icechunk gets this length from the pinned chunk reference, not
            // an extra payload HEAD. Missing chunk references have size zero.
            let size = self
                .0
                .size_key(key)
                .await
                .map_err(storage_error)?
                .unwrap_or(0);
            encoded.object(key.as_str(), size)?;
            let bytes = self.0.get(key).await.map_err(storage_error)?;
            if bytes
                .as_ref()
                .is_some_and(|bytes| bytes.len() as u64 > size)
            {
                return Err(DataServerError::ResourceExhausted);
            }
            Ok(bytes)
        })
        .map_err(io_err)
    }

    fn get_partial_many<'a>(
        &'a self,
        key: &StoreKey,
        ranges: ByteRangeIterator<'a>,
    ) -> Result<MaybeBytesIterator<'a>, StorageError> {
        let encoded = crate::encoded::current();
        let ranges: Vec<_> = ranges.collect();
        // Creation and consumption of the stream share one absolute deadline.
        let bytes = runtime::run(async {
            let admitted = if let Some(encoded) = encoded {
                let size = self
                    .0
                    .size_key(key)
                    .await
                    .map_err(storage_error)?
                    .unwrap_or(0);
                // getsize reports zero for an absent chunk. The upstream
                // multi-range stream otherwise turns absence into a per-item
                // error; preserve the fill-chunk semantics of get().
                if size == 0 && self.0.get(key).await.map_err(storage_error)?.is_none() {
                    return Ok(None);
                }
                // Reserve the entire operation before Icechunk launches any
                // range futures or collects their results.
                let length = ranges.iter().try_fold(0u64, |total, range| {
                    total
                        .checked_add(range_length(*range, size)?)
                        .ok_or(DataServerError::ResourceExhausted)
                })?;
                encoded.ranges(length)?;
                Some(length)
            } else {
                None
            };
            match self
                .0
                .get_partial_many(key, Box::new(ranges.into_iter()))
                .await
                .map_err(storage_error)?
            {
                Some(stream) => {
                    let bytes: Vec<bytes::Bytes> =
                        stream.try_collect().await.map_err(storage_error)?;
                    let actual = bytes
                        .iter()
                        .try_fold(0u64, |n, bytes| n.checked_add(bytes.len() as u64))
                        .ok_or(DataServerError::ResourceExhausted)?;
                    if admitted.is_some_and(|length| actual > length) {
                        return Err(DataServerError::ResourceExhausted);
                    }
                    Ok(Some(bytes))
                }
                None => Ok(None),
            }
        })
        .map_err(io_err)?;
        Ok(bytes.map(|bytes| Box::new(bytes.into_iter().map(Ok)) as _))
    }

    fn size_key(&self, key: &StoreKey) -> Result<Option<u64>, StorageError> {
        storage_call(self.0.size_key(key))
    }

    fn supports_get_partial(&self) -> bool {
        true
    }
}

fn storage_error(error: StorageError) -> DataServerError {
    DataServerError::Storage(error.to_string())
}

fn range_length(
    range: zarrs::storage::byte_range::ByteRange,
    size: u64,
) -> Result<u64, DataServerError> {
    use zarrs::storage::byte_range::ByteRange;
    match range {
        ByteRange::Suffix(length) => Ok(length.min(size)),
        ByteRange::FromStart(start, length) => {
            let end = match length {
                Some(length) => start
                    .checked_add(length)
                    .ok_or(DataServerError::ResourceExhausted)?
                    .min(size),
                None => size,
            };
            // A missing chunk reference has size zero. Let the backend retain
            // its missing-key/range semantics without rejecting fill chunks.
            Ok(end.saturating_sub(start))
        }
    }
}

impl ListableStorageTraits for Store {
    fn list(&self) -> Result<StoreKeys, StorageError> {
        storage_call(self.0.list())
    }
    fn list_prefix(&self, prefix: &StorePrefix) -> Result<StoreKeys, StorageError> {
        storage_call(self.0.list_prefix(prefix))
    }
    fn list_dir(&self, prefix: &StorePrefix) -> Result<StoreKeysPrefixes, StorageError> {
        storage_call(self.0.list_dir(prefix))
    }
    fn size_prefix(&self, prefix: &StorePrefix) -> Result<u64, StorageError> {
        storage_call(self.0.size_prefix(prefix))
    }
}

/// Build the Icechunk object-storage backend (S3 or local) for the repo.
async fn build_storage(
    collection_id: &str,
    config: &ZarrConfig,
) -> Result<Arc<dyn icechunk::storage::Storage + Send + Sync>, DataServerError> {
    let cfg_err =
        |msg: String| DataServerError::Config(format!("Collection '{collection_id}': {msg}"));
    let ic = config.icechunk.as_ref().expect("icechunk config present");

    if let (Some(endpoint), Some(bucket)) = (config.endpoint.as_deref(), config.bucket.as_deref()) {
        // S3-compatible repo. `path` is the repo root within the bucket
        // (required for the remote source — config-validated). Access is
        // **anonymous** (public datasets only); authenticated/private repos are
        // a v1 non-goal (#335).
        let prefix = config.path.clone();
        let mut opts = icechunk::storage::S3Options::default()
            .with_endpoint_url(endpoint)
            // Path-style by default (S3-compatible + AWS regional endpoints);
            // override per config for virtual-host-style.
            .with_force_path_style(ic.force_path_style.unwrap_or(true))
            .with_allow_http(endpoint.starts_with("http://"))
            // The object_store S3 backend keys anonymous access off
            // `S3Options.anonymous` (→ `skip_signature`), NOT the `credentials`
            // arg below. Without this it falls through to the AWS credential
            // chain (env → profile → EC2 IMDS) and hangs/fails off-EC2. Public
            // datasets only (authenticated repos are a v1 non-goal, #335).
            // TODO(#335): make this conditional if private-repo support is added —
            // an unconditional `with_anonymous(true)` would silently suppress any
            // configured credentials and connect unsigned.
            .with_anonymous(true);
        if let Some(region) = ic.region.as_deref() {
            opts = opts.with_region(region);
        }
        // Use icechunk's `object_store`-based S3 backend (the same `object_store`
        // crate `ds-storage` uses) rather than `new_s3_storage` (the `aws-sdk-s3`
        // backend) — avoids pulling the whole AWS SDK. No-signing is set on
        // `opts` via `with_anonymous(true)` above (this backend keys off
        // `S3Options.anonymous`); the `credentials` arg is ignored by this
        // backend, so pass `None`. Public datasets only; authenticated/private
        // repos are a v1 non-goal (#335).
        icechunk::storage::new_s3_object_store_storage(
            opts,
            bucket.to_string(),
            prefix,
            // Ignored by the object_store backend (anonymity is set above); `None`
            // rather than a vestigial `Some(Anonymous)` to avoid implying it has
            // an effect. See #335 if private-repo credentials are added.
            None,
            // No extra read/write headers.
            Vec::new(),
            Vec::new(),
        )
        .await
        .map_err(|e| cfg_err(format!("failed to build Icechunk S3 storage: {e}")))
    } else if let Some(data_path) = config.data_path.as_deref() {
        // The local backend is a real directory only. A URL in `data_path`
        // (which plain Zarr accepts) is not supported for Icechunk — use
        // `endpoint`+`bucket` for S3.
        if data_path.contains("://") {
            return Err(cfg_err(format!(
                "icechunk 'data_path' must be a local directory, not a URL ('{data_path}'); \
                 use 'endpoint'+'bucket' for an S3 repo"
            )));
        }
        let root = match &config.path {
            Some(p) => format!(
                "{}/{}",
                data_path.trim_end_matches('/'),
                p.trim_matches('/')
            ),
            None => data_path.to_string(),
        };
        icechunk::storage::new_local_filesystem_storage(std::path::Path::new(&root))
            .await
            .map_err(|e| {
                cfg_err(format!(
                    "failed to build Icechunk local storage '{root}': {e}"
                ))
            })
    } else {
        Err(cfg_err(
            "icechunk requires 'data_path' (local) or 'endpoint'+'bucket' (S3)".into(),
        ))
    }
}

/// Resolve the configured version selector to an Icechunk [`VersionInfo`]
/// (default: HEAD of branch `main`).
fn version_info(collection_id: &str, ic: &IcechunkConfig) -> Result<VersionInfo, DataServerError> {
    if let Some(snapshot) = &ic.snapshot {
        let id = icechunk::format::SnapshotId::try_from(snapshot.as_str()).map_err(|e| {
            DataServerError::Config(format!(
                "Collection '{collection_id}': invalid icechunk snapshot id '{snapshot}': {e}"
            ))
        })?;
        Ok(VersionInfo::SnapshotId(id))
    } else if let Some(tag) = &ic.tag {
        Ok(VersionInfo::TagRef(tag.clone()))
    } else {
        Ok(VersionInfo::BranchTipRef(
            ic.branch.clone().unwrap_or_else(|| "main".to_string()),
        ))
    }
}

#[cfg(test)]
mod tests;
