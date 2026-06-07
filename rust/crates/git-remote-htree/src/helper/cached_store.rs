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
            let computed = Sha256::digest(&data);
            if computed[..] == hash[..] {
                return Ok(Some(data));
            }
            debug!(
                "Ignoring corrupt local cache entry for {}",
                hex::encode(hash)
            );
        }

        let mut last_result = Ok(None);
        for attempt in 0..4 {
            let result = self.blossom.get(hash).await;
            match result {
                Ok(Some(data)) => {
                    let _ = self.local.put(*hash, data.clone()).await;
                    return Ok(Some(data));
                }
                Ok(None) if attempt < 3 => {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        100 * (attempt + 1) as u64,
                    ))
                    .await;
                    last_result = Ok(None);
                }
                Ok(None) => return Ok(None),
                Err(err) if attempt < 3 => {
                    last_result = Err(err);
                    tokio::time::sleep(std::time::Duration::from_millis(
                        100 * (attempt + 1) as u64,
                    ))
                    .await;
                }
                Err(err) => return Err(err),
            }
        }

        last_result
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
