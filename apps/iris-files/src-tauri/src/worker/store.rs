//! Blob store wrapper using hashtree-fs for filesystem storage.
//!
//! Provides a hex-string API for worker commands while using FsBlobStore
//! from hashtree-fs for the actual storage implementation.

use hashtree_fs::FsBlobStore;
use std::path::PathBuf;
use std::sync::Arc;

/// Default max storage: 1GB
const DEFAULT_MAX_BYTES: u64 = 1024 * 1024 * 1024;

/// Wrapper around FsBlobStore providing hex-string API for worker commands.
/// The underlying FsBlobStore implements hashtree_core::Store directly.
pub struct BlobStore {
    inner: Arc<FsBlobStore>,
}

impl BlobStore {
    pub fn new(data_dir: PathBuf) -> Self {
        let blobs_path = data_dir.join("blobs");
        let store = FsBlobStore::with_max_bytes(&blobs_path, DEFAULT_MAX_BYTES)
            .expect("Failed to create blob store");
        Self {
            inner: Arc::new(store),
        }
    }

    /// Get the underlying FsBlobStore for use with HashTree
    pub fn inner(&self) -> Arc<FsBlobStore> {
        self.inner.clone()
    }

    /// Set maximum storage size in bytes
    pub fn set_max_bytes(&self, max: u64) {
        use hashtree_core::Store;
        self.inner.set_max_bytes(max);
    }

    /// Get current maximum storage size
    pub fn max_bytes(&self) -> u64 {
        use hashtree_core::Store;
        self.inner.max_bytes().unwrap_or(0)
    }

    /// Evict oldest blobs if storage exceeds limit
    pub async fn evict_if_needed(&self) -> u64 {
        use hashtree_core::Store;
        self.inner.evict_if_needed().await.unwrap_or(0)
    }

    /// Get blob by hex-encoded hash
    pub async fn get(&self, hash_hex: &str) -> Option<Vec<u8>> {
        let hash = hex_to_hash(hash_hex)?;
        use hashtree_core::Store;
        self.inner.get(&hash).await.ok().flatten()
    }

    /// Store blob with hex-encoded hash
    pub async fn put(&self, hash_hex: &str, data: &[u8]) -> Result<bool, String> {
        let hash = hex_to_hash(hash_hex).ok_or("Invalid hash hex")?;
        use hashtree_core::Store;
        self.inner
            .put(hash, data.to_vec())
            .await
            .map_err(|e| e.to_string())
    }

    /// Check if blob exists
    pub fn has(&self, hash_hex: &str) -> bool {
        let Some(hash) = hex_to_hash(hash_hex) else {
            return false;
        };
        self.inner.exists(&hash)
    }

    /// Delete blob by hash
    pub async fn delete(&self, hash_hex: &str) -> bool {
        let Some(hash) = hex_to_hash(hash_hex) else {
            return false;
        };
        use hashtree_core::Store;
        self.inner.delete(&hash).await.unwrap_or(false)
    }

    /// Pin a hash (increment ref count). Pinned items are not evicted.
    pub async fn pin(&self, hash_hex: &str) -> Result<(), String> {
        let hash = hex_to_hash(hash_hex).ok_or("Invalid hash hex")?;
        use hashtree_core::Store;
        self.inner.pin(&hash).await.map_err(|e| e.to_string())
    }

    /// Unpin a hash (decrement ref count). Item can be evicted when count reaches 0.
    pub async fn unpin(&self, hash_hex: &str) -> Result<(), String> {
        let hash = hex_to_hash(hash_hex).ok_or("Invalid hash hex")?;
        use hashtree_core::Store;
        self.inner.unpin(&hash).await.map_err(|e| e.to_string())
    }

    /// Get pin count for a hash. 0 = not pinned.
    pub fn pin_count(&self, hash_hex: &str) -> u32 {
        let Some(hash) = hex_to_hash(hash_hex) else {
            return 0;
        };
        use hashtree_core::Store;
        self.inner.pin_count(&hash)
    }

    /// Check if hash is pinned (pin count > 0)
    pub fn is_pinned(&self, hash_hex: &str) -> bool {
        self.pin_count(hash_hex) > 0
    }

    /// Get storage statistics
    pub fn stats(&self) -> StorageStats {
        let fs_stats = self.inner.stats().unwrap_or_else(|_| hashtree_fs::FsStats {
            count: 0,
            total_bytes: 0,
            pinned_count: 0,
            pinned_bytes: 0,
        });
        StorageStats {
            items: fs_stats.count as u64,
            bytes: fs_stats.total_bytes,
            pinned_items: fs_stats.pinned_count as u64,
            pinned_bytes: fs_stats.pinned_bytes,
        }
    }
}

/// Convert hex string to 32-byte hash
fn hex_to_hash(hex: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(hex).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&bytes);
    Some(hash)
}

/// Storage statistics
#[derive(Debug, Clone)]
pub struct StorageStats {
    pub items: u64,
    pub bytes: u64,
    pub pinned_items: u64,
    pub pinned_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_put_and_get() {
        let dir = tempdir().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf());

        // Use a valid SHA256 hash (64 hex chars)
        let hash = "a".repeat(64);
        let data = b"Hello, World!";

        // Put data
        let ok = store.put(&hash, data).await.unwrap();
        assert!(ok);

        // Get data
        let result = store.get(&hash).await;
        assert_eq!(result, Some(data.to_vec()));
    }

    #[tokio::test]
    async fn test_has() {
        let dir = tempdir().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf());

        let hash = "b".repeat(64);

        // Should not exist initially
        assert!(!store.has(&hash));

        // Put data
        store.put(&hash, b"data").await.unwrap();

        // Should exist now
        assert!(store.has(&hash));
    }

    #[tokio::test]
    async fn test_get_nonexistent() {
        let dir = tempdir().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf());

        let result = store.get(&"c".repeat(64)).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_delete() {
        let dir = tempdir().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf());

        let hash = "d".repeat(64);
        store.put(&hash, b"delete me").await.unwrap();
        assert!(store.has(&hash));

        let deleted = store.delete(&hash).await;
        assert!(deleted);
        assert!(!store.has(&hash));
    }

    #[tokio::test]
    async fn test_delete_nonexistent() {
        let dir = tempdir().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf());

        let deleted = store.delete(&"e".repeat(64)).await;
        assert!(!deleted);
    }

    #[tokio::test]
    async fn test_stats() {
        let dir = tempdir().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf());

        let hash = "f".repeat(64);
        store.put(&hash, b"test data").await.unwrap();

        let stats = store.stats();
        assert_eq!(stats.items, 1);
        assert_eq!(stats.bytes, 9); // "test data" = 9 bytes
    }

    #[tokio::test]
    async fn test_pin_and_unpin() {
        let dir = tempdir().unwrap();
        let store = BlobStore::new(dir.path().to_path_buf());

        let hash = "0".repeat(64);
        store.put(&hash, b"pin me").await.unwrap();

        // Initially not pinned
        assert!(!store.is_pinned(&hash));
        assert_eq!(store.pin_count(&hash), 0);

        // Pin
        store.pin(&hash).await.unwrap();
        assert!(store.is_pinned(&hash));
        assert_eq!(store.pin_count(&hash), 1);

        // Unpin
        store.unpin(&hash).await.unwrap();
        assert!(!store.is_pinned(&hash));
        assert_eq!(store.pin_count(&hash), 0);
    }
}
