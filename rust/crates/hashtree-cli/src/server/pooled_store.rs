//! Store adapter that keeps blob reads off the async workers.
//!
//! `StorageRouter` reads are synchronous (LMDB, optionally an S3 fallback).
//! Driving a `HashTree` over it directly runs that I/O on a tokio worker.
//! This adapter sends each read through the same bounded blocking pool the
//! raw blob routes use, so tree reads obey the same admission limits.

use super::blob_read::{run_blob_metadata_read, run_blob_read, BlobIoTaskError, BLOB_READ_BUSY};
use crate::storage::StorageRouter;
use async_trait::async_trait;
use futures::executor::block_on;
use hashtree_core::{Hash, Store, StoreError};
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct PooledReadStore {
    inner: Arc<StorageRouter>,
}

impl PooledReadStore {
    pub(super) fn new(inner: Arc<StorageRouter>) -> Self {
        Self { inner }
    }
}

fn pooled<T>(result: Result<Result<T, StoreError>, BlobIoTaskError>) -> Result<T, StoreError> {
    match result {
        Ok(result) => result,
        Err(error) if error.is_busy() => Err(StoreError::Other(BLOB_READ_BUSY.to_string())),
        Err(error) if error.is_timeout() => {
            Err(StoreError::Other(format!("blob read timed out: {error}")))
        }
        Err(error) => Err(StoreError::Other(format!("blob read task failed: {error}"))),
    }
}

// The router's async methods finish without yielding, so `block_on` inside the
// blocking task only supplies the executor they nominally need.
#[async_trait]
impl Store for PooledReadStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.inner.put(hash, data).await
    }

    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.inner.put_many(items).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let (inner, hash) = (Arc::clone(&self.inner), *hash);
        pooled(run_blob_read(move || block_on(inner.get(&hash))).await)
    }

    async fn get_range(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let (inner, hash) = (Arc::clone(&self.inner), *hash);
        pooled(run_blob_read(move || block_on(inner.get_range(&hash, start, end_inclusive))).await)
    }

    async fn blob_size(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        let (inner, hash) = (Arc::clone(&self.inner), *hash);
        pooled(run_blob_metadata_read(move || block_on(inner.blob_size(&hash))).await)
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        let (inner, hash) = (Arc::clone(&self.inner), *hash);
        pooled(run_blob_metadata_read(move || block_on(inner.has(&hash))).await)
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.delete(hash).await
    }
}
