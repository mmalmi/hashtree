//! Read-through store for HTTP tree requests. Local and upstream reads use
//! the same bounded storage pool and verified blob path as raw HTTP reads.

use super::auth::AppState;
use super::handlers::{
    fetch_blob_for_tree, get_blob_range_without_blocking_runtime,
    get_blob_size_without_blocking_runtime, get_blob_without_blocking_runtime,
};
use async_trait::async_trait;
use bytes::Bytes;
use hashtree_core::{store::slice_blob_range, Hash, Store, StoreError};

#[derive(Clone)]
pub(super) struct PooledReadStore {
    state: AppState,
}

impl PooledReadStore {
    pub(super) fn new(state: AppState) -> Self {
        Self { state }
    }

    async fn read_upstream(&self, hash: &Hash) -> Result<Option<Bytes>, StoreError> {
        fetch_blob_for_tree(&self.state, hash)
            .await
            .map_err(StoreError::Other)
    }
}

#[async_trait]
impl Store for PooledReadStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.state.store.store_arc().put(hash, data).await
    }

    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.state.store.store_arc().put_many(items).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        if let Some(data) = get_blob_without_blocking_runtime(&self.state, *hash)
            .await
            .map_err(StoreError::Other)?
        {
            return Ok(Some(data));
        }
        Ok(self.read_upstream(hash).await?.map(|data| data.to_vec()))
    }

    async fn get_range(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        if let Some(data) =
            get_blob_range_without_blocking_runtime(&self.state, *hash, start, end_inclusive)
                .await
                .map_err(StoreError::Other)?
        {
            return Ok(Some(data));
        }
        self.read_upstream(hash)
            .await?
            .map(|data| slice_blob_range(&data, start, end_inclusive))
            .transpose()
    }

    async fn blob_size(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        if let Some(size) = get_blob_size_without_blocking_runtime(&self.state, *hash)
            .await
            .map_err(StoreError::Other)?
        {
            return Ok(Some(size));
        }
        Ok(self
            .read_upstream(hash)
            .await?
            .map(|data| data.len() as u64))
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        Ok(self.blob_size(hash).await?.is_some())
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.state.store.store_arc().delete(hash).await
    }
}
