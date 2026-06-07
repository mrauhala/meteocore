//! Icechunk source for the Zarr engine (feature `icechunk`, issue #335).
//!
//! Opens an Icechunk repository (transactional/versioned Zarr) read-only at a
//! chosen version, exposes it through `zarrs_icechunk::AsyncIcechunkStore`, and
//! bridges that **async** store to the engine's **sync** read path via
//! `AsyncToSyncStorageAdapter`.
//!
//! The bridge blocks on the calling thread exactly like `ds-storage` does
//! (`block_in_place` inside a runtime, a temporary runtime otherwise). Combined
//! with the engine's `concurrent_target(1)` retrieval, storage reads never land
//! on a `rayon` worker — the same invariant that keeps the plain backend safe.
//!
//! Icechunk owns its own object storage (S3/local), so this path does **not**
//! go through `ds-storage` (a deliberate deviation — Icechunk is the storage
//! engine).

use std::collections::HashMap;
use std::sync::Arc;

use icechunk::repository::VersionInfo;
use icechunk::Repository;
use zarrs::storage::storage_adapter::async_to_sync::{
    AsyncToSyncBlockOn, AsyncToSyncStorageAdapter,
};
use zarrs_icechunk::AsyncIcechunkStore;

use ds_core::config::{IcechunkConfig, ZarrConfig};
use ds_core::error::DataServerError;

use crate::store::EngineStore;

/// Drives async futures to completion from the engine's sync read path. Mirrors
/// `ds-storage`'s bridge: `block_in_place` when already inside a multi-thread
/// runtime (request/poll worker), a temporary runtime when there is none
/// (tests). Safe because retrieval is pinned to `concurrent_target(1)`, so this
/// is never invoked from a `rayon` worker (where `block_in_place` would panic).
struct TokioBlockOn;

impl AsyncToSyncBlockOn for TokioBlockOn {
    fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
            Err(_) => tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("build temporary tokio runtime")
                .block_on(future),
        }
    }
}

/// Build an [`EngineStore`] backed by an Icechunk repository at the configured
/// version.
pub fn build_store(
    collection_id: &str,
    config: &ZarrConfig,
) -> Result<EngineStore, DataServerError> {
    let ic = config
        .icechunk
        .as_ref()
        .expect("build_store called without [zarr.icechunk]");

    let cfg_err =
        |msg: String| DataServerError::Config(format!("Collection '{collection_id}': {msg}"));

    // All repository operations are async; drive them on the calling thread.
    let session = TokioBlockOn.block_on(async {
        let storage = build_storage(collection_id, config).await?;
        let repo = Repository::open(None, storage, HashMap::new())
            .await
            .map_err(|e| cfg_err(format!("failed to open Icechunk repository: {e}")))?;
        let version = version_info(collection_id, ic)?;
        repo.readonly_session(&version).await.map_err(|e| {
            cfg_err(format!(
                "failed to open Icechunk session ({version:?}): {e}"
            ))
        })
    })?;

    let async_store = Arc::new(AsyncIcechunkStore::new(session));
    let sync_store = AsyncToSyncStorageAdapter::new(async_store, TokioBlockOn);
    Ok(EngineStore::new(sync_store))
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
