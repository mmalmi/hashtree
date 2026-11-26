use hashtree_blossom::BlossomStore;
use hashtree_core::{Hash, Store, StoreError};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tracing::debug;

pub(super) struct CachedStore {
    local: Arc<dyn Store + Send + Sync>,
    blossom: BlossomStore,
}

impl CachedStore {
    pub(super) fn new(local: Arc<dyn Store + Send + Sync>, blossom: BlossomStore) -> Self {
        Self { local, blossom }
    }
}

#[async_trait::async_trait]
impl Store for CachedStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.local.put(hash, data).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        if let Ok(Some(data)) = self.local.get(hash).await {
            if Sha256::digest(&data)[..] == hash[..] {
                return Ok(Some(data));
            }
            debug!(
                "Ignoring corrupt local cache entry for {}",
                hex::encode(hash)
            );
        }

        let result = self.blossom.get(hash).await?;
        if let Some(ref data) = result {
            let _ = self.local.put(*hash, data.clone()).await;
        }
        Ok(result)
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        if self.local.has(hash).await? {
            return Ok(true);
        }
        self.blossom.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.local.delete(hash).await
    }
}
