//! Combined store that checks local filesystem first, then Blossom
//!
//! This allows tree operations to fetch blobs from Blossom if not cached locally.

use async_trait::async_trait;
use hashtree_blossom::{BlossomClient, BlossomStore};
use hashtree_core::{to_hex, Store, StoreError};
use hashtree_fs::FsBlobStore;
use nostr_sdk::Keys;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// Default Blossom servers for fetching blobs
const DEFAULT_BLOSSOM_SERVERS: &[&str] = &[
    "https://cdn.iris.to",
];

/// Combined store that checks local filesystem first, then Blossom
pub struct CombinedStore {
    local: Arc<FsBlobStore>,
    blossom: Arc<RwLock<BlossomStore>>,
}

impl CombinedStore {
    pub fn new(local: Arc<FsBlobStore>) -> Self {
        // Create default Blossom store with anonymous keys for read-only access
        let keys = Keys::generate();
        let blossom_client = BlossomClient::new_empty(keys)
            .with_read_servers(DEFAULT_BLOSSOM_SERVERS.iter().map(|s| s.to_string()).collect());
        let blossom_store = BlossomStore::new(blossom_client);

        Self {
            local,
            blossom: Arc::new(RwLock::new(blossom_store)),
        }
    }

    /// Update Blossom read servers
    pub async fn set_blossom_servers(&self, read_servers: Vec<String>, keys: Option<Keys>) {
        let keys = keys.unwrap_or_else(Keys::generate);
        let blossom_client = BlossomClient::new_empty(keys)
            .with_read_servers(read_servers);
        let mut guard = self.blossom.write().await;
        *guard = BlossomStore::new(blossom_client);
    }
}

#[async_trait]
impl Store for CombinedStore {
    async fn get(&self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>, StoreError> {
        // Try local store first
        if let Ok(Some(data)) = self.local.get(hash).await {
            debug!("Found blob {} in local store ({} bytes)", &to_hex(hash)[..8], data.len());
            return Ok(Some(data));
        }

        // Fall back to Blossom
        let blossom = self.blossom.read().await;
        match blossom.get(hash).await {
            Ok(Some(data)) => {
                debug!("Found blob {} in Blossom ({} bytes)", &to_hex(hash)[..8], data.len());
                // Cache locally for future requests
                drop(blossom); // Release read lock before writing
                match self.local.put(*hash, data.clone()).await {
                    Ok(_) => debug!("Cached blob {} locally", &to_hex(hash)[..8]),
                    Err(e) => warn!("Failed to cache blob locally: {}", e),
                }
                Ok(Some(data))
            }
            Ok(None) => {
                debug!("Blob {} not found in local or Blossom", &to_hex(hash)[..8]);
                Ok(None)
            }
            Err(e) => {
                warn!("Blossom fetch error for {}: {}", &to_hex(hash)[..8], e);
                Err(StoreError::Other(e.to_string()))
            }
        }
    }

    async fn put(&self, hash: [u8; 32], data: Vec<u8>) -> Result<bool, StoreError> {
        self.local.put(hash, data).await
    }

    async fn has(&self, hash: &[u8; 32]) -> Result<bool, StoreError> {
        // Check local first
        if self.local.has(hash).await? {
            return Ok(true);
        }

        // Check Blossom
        let blossom = self.blossom.read().await;
        blossom
            .has(hash)
            .await
            .map_err(|e| StoreError::Other(e.to_string()))
    }

    async fn delete(&self, hash: &[u8; 32]) -> Result<bool, StoreError> {
        self.local.delete(hash).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_combined_store_local_only() {
        let dir = tempdir().unwrap();
        let local = Arc::new(FsBlobStore::new(dir.path()).unwrap());
        let store = CombinedStore::new(local);

        // Put data locally
        let hash = [0xaa; 32];
        store.put(hash, b"test data".to_vec()).await.unwrap();

        // Should find in local
        let data = store.get(&hash).await.unwrap();
        assert_eq!(data, Some(b"test data".to_vec()));
    }

    #[tokio::test]
    async fn test_combined_store_blossom_fallback() {
        // Test fetching the known media tree root from Blossom
        let dir = tempdir().unwrap();
        let local = Arc::new(FsBlobStore::new(dir.path()).unwrap());
        let store = CombinedStore::new(local.clone());

        // This hash is the media tree root that exists on cdn.iris.to
        let hash_hex = "e4190b9acd45e5d4675f0a46447a63aa155646d77f734f2c3940184b9a877671";
        let hash: [u8; 32] = hex::decode(hash_hex).unwrap().try_into().unwrap();

        println!("Fetching blob from Blossom via CombinedStore...");
        let result = store.get(&hash).await;
        match result {
            Ok(Some(data)) => {
                println!("SUCCESS! Got {} bytes", data.len());
                println!("First 64 bytes: {:?}", &data[..64.min(data.len())]);

                // Verify it was cached locally
                let local_data = local.get(&hash).await.unwrap();
                assert!(local_data.is_some(), "Should be cached locally");
            }
            Ok(None) => {
                panic!("Blob not found - check if Blossom servers are accessible");
            }
            Err(e) => {
                panic!("Error fetching blob: {}", e);
            }
        }
    }
}
