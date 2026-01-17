//! LMDB-backed content-addressed blob storage.

use async_trait::async_trait;
use heed::types::*;
use heed::{Database, EnvOpenOptions};
use hashtree_core::store::{Store, StoreError};
use hashtree_core::types::Hash;
use std::path::Path;

// Re-export sha256 for convenience
pub use hashtree_core::hash::sha256 as compute_sha256;

/// LMDB-backed blob store implementing hashtree's Store trait.
pub struct LmdbBlobStore {
    env: heed::Env,
    /// Maps SHA256 hash (32 bytes) → blob data
    blobs: Database<Bytes, Bytes>,
}

impl LmdbBlobStore {
    /// Open or create an LMDB blob store at the given path.
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, StoreError> {
        Self::with_map_size(path, 10 * 1024 * 1024 * 1024) // 10GB default
    }

    /// Open or create with custom map size.
    pub fn with_map_size<P: AsRef<Path>>(path: P, map_size: usize) -> Result<Self, StoreError> {
        std::fs::create_dir_all(&path).map_err(StoreError::Io)?;

        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(map_size)
                .max_dbs(1)
                .open(path)
                .map_err(|e| StoreError::Other(e.to_string()))?
        };

        let mut wtxn = env
            .write_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let blobs = env
            .create_database(&mut wtxn, Some("blobs"))
            .map_err(|e| StoreError::Other(e.to_string()))?;
        wtxn.commit()
            .map_err(|e| StoreError::Other(e.to_string()))?;

        Ok(Self { env, blobs })
    }

    /// Check if a hash exists (sync version for internal use).
    pub fn exists(&self, hash: &Hash) -> Result<bool, StoreError> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;

        Ok(self
            .blobs
            .get(&rtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .is_some())
    }

    /// Get storage statistics.
    pub fn stats(&self) -> Result<LmdbStats, StoreError> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;

        let count = self
            .blobs
            .len(&rtxn)
            .map_err(|e| StoreError::Other(e.to_string()))?
            as usize;

        let mut total_bytes = 0u64;
        for item in self
            .blobs
            .iter(&rtxn)
            .map_err(|e| StoreError::Other(e.to_string()))?
        {
            let (_, data) = item.map_err(|e| StoreError::Other(e.to_string()))?;
            total_bytes += data.len() as u64;
        }

        Ok(LmdbStats { count, total_bytes })
    }

    /// List all hashes in the store.
    pub fn list(&self) -> Result<Vec<Hash>, StoreError> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;

        let mut hashes = Vec::new();
        for item in self
            .blobs
            .iter(&rtxn)
            .map_err(|e| StoreError::Other(e.to_string()))?
        {
            let (hash, _) = item.map_err(|e| StoreError::Other(e.to_string()))?;
            let hash_arr: Hash = hash
                .try_into()
                .map_err(|_| StoreError::Other("invalid hash length".into()))?;
            hashes.push(hash_arr);
        }

        Ok(hashes)
    }

    /// Sync put operation (for use in sync contexts).
    pub fn put_sync(&self, hash: Hash, data: &[u8]) -> Result<bool, StoreError> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;

        let existed = self
            .blobs
            .get(&wtxn, &hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .is_some();

        if !existed {
            self.blobs
                .put(&mut wtxn, &hash, data)
                .map_err(|e| StoreError::Other(e.to_string()))?;
        }

        wtxn.commit()
            .map_err(|e| StoreError::Other(e.to_string()))?;

        Ok(!existed)
    }

    /// Sync get operation (for use in sync contexts).
    pub fn get_sync(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;

        Ok(self
            .blobs
            .get(&rtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .map(|b| b.to_vec()))
    }

    /// Sync delete operation (for use in sync contexts).
    pub fn delete_sync(&self, hash: &Hash) -> Result<bool, StoreError> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;

        let existed = self
            .blobs
            .delete(&mut wtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?;

        wtxn.commit()
            .map_err(|e| StoreError::Other(e.to_string()))?;

        Ok(existed)
    }
}

#[derive(Debug, Clone)]
pub struct LmdbStats {
    pub count: usize,
    pub total_bytes: u64,
}

#[async_trait]
impl Store for LmdbBlobStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.put_sync(hash, &data)
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.get_sync(hash)
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.exists(hash)
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.delete_sync(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_core::sha256;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_put_get() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;

        let data = b"hello lmdb";
        let hash = sha256(data);
        store.put(hash, data.to_vec()).await?;

        assert!(store.has(&hash).await?);
        assert_eq!(store.get(&hash).await?, Some(data.to_vec()));

        Ok(())
    }

    #[tokio::test]
    async fn test_delete() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;

        let data = b"delete me";
        let hash = sha256(data);
        store.put(hash, data.to_vec()).await?;
        assert!(store.has(&hash).await?);

        assert!(store.delete(&hash).await?);
        assert!(!store.has(&hash).await?);
        assert!(!store.delete(&hash).await?);

        Ok(())
    }

    #[tokio::test]
    async fn test_list() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;

        let d1 = b"one";
        let d2 = b"two";
        let d3 = b"three";
        let h1 = sha256(d1);
        let h2 = sha256(d2);
        let h3 = sha256(d3);

        store.put(h1, d1.to_vec()).await?;
        store.put(h2, d2.to_vec()).await?;
        store.put(h3, d3.to_vec()).await?;

        let hashes = store.list()?;
        assert_eq!(hashes.len(), 3);
        assert!(hashes.contains(&h1));
        assert!(hashes.contains(&h2));
        assert!(hashes.contains(&h3));

        Ok(())
    }

    #[tokio::test]
    async fn test_stats() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;

        let d1 = b"hello";
        let d2 = b"world";
        store.put(sha256(d1), d1.to_vec()).await?;
        store.put(sha256(d2), d2.to_vec()).await?;

        let stats = store.stats()?;
        assert_eq!(stats.count, 2);
        assert_eq!(stats.total_bytes, 10);

        Ok(())
    }

    #[tokio::test]
    async fn test_deduplication() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;

        let data = b"same";
        let hash = sha256(data);
        assert!(store.put(hash, data.to_vec()).await?); // Returns true (newly stored)
        assert!(!store.put(hash, data.to_vec()).await?); // Returns false (already existed)

        assert_eq!(store.list()?.len(), 1);

        Ok(())
    }
}
