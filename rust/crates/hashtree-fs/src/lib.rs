//! Filesystem-based content-addressed blob storage.
//!
//! Stores blobs in a directory structure similar to git:
//! `{base_path}/{first 2 chars of hash}/{remaining hash chars}`
//!
//! For example, a blob with hash `abcdef123...` would be stored at:
//! `~/.hashtree/blobs/ab/cdef123...`

use async_trait::async_trait;
use hashtree_core::store::{Store, StoreError, StoreStats};
use hashtree_core::types::Hash;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::SystemTime;

/// Filesystem-backed blob store implementing hashtree's Store trait.
///
/// Stores blobs in a 256-way sharded directory structure using
/// the first 2 hex characters of the hash as the directory prefix.
/// Supports storage limits with mtime-based FIFO eviction and pinning.
pub struct FsBlobStore {
    base_path: PathBuf,
    max_bytes: AtomicU64,
    /// Pin counts stored in memory, persisted to pins.json
    pins: RwLock<HashMap<String, u32>>,
}

impl FsBlobStore {
    /// Create a new filesystem blob store at the given path.
    ///
    /// Creates the directory if it doesn't exist.
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, StoreError> {
        let base_path = path.as_ref().to_path_buf();
        fs::create_dir_all(&base_path)?;

        // Load existing pins from disk
        let pins = Self::load_pins(&base_path).unwrap_or_default();

        Ok(Self {
            base_path,
            max_bytes: AtomicU64::new(0), // 0 = unlimited
            pins: RwLock::new(pins),
        })
    }

    /// Create a new store with a maximum size limit
    pub fn with_max_bytes<P: AsRef<Path>>(path: P, max_bytes: u64) -> Result<Self, StoreError> {
        let store = Self::new(path)?;
        store.max_bytes.store(max_bytes, Ordering::Relaxed);
        Ok(store)
    }

    /// Path to pins.json file
    fn pins_path(&self) -> PathBuf {
        self.base_path.join("pins.json")
    }

    /// Load pins from disk
    fn load_pins(base_path: &Path) -> Option<HashMap<String, u32>> {
        let pins_path = base_path.join("pins.json");
        let contents = fs::read_to_string(pins_path).ok()?;
        serde_json::from_str(&contents).ok()
    }

    /// Save pins to disk
    fn save_pins(&self) -> Result<(), StoreError> {
        let pins = self.pins.read().unwrap();
        let json = serde_json::to_string(&*pins)
            .map_err(|e| StoreError::Other(format!("Failed to serialize pins: {}", e)))?;
        fs::write(self.pins_path(), json)?;
        Ok(())
    }

    /// Get the file path for a given hash.
    ///
    /// Format: `{base_path}/{first 2 hex chars}/{remaining 62 hex chars}`
    fn blob_path(&self, hash: &Hash) -> PathBuf {
        let hex = hex::encode(hash);
        let (prefix, rest) = hex.split_at(2);
        self.base_path.join(prefix).join(rest)
    }

    /// Sync put operation.
    pub fn put_sync(&self, hash: Hash, data: &[u8]) -> Result<bool, StoreError> {
        let path = self.blob_path(&hash);

        // Check if already exists
        if path.exists() {
            return Ok(false);
        }

        // Create parent directory if needed
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Write atomically using temp file + rename
        let temp_path = path.with_extension("tmp");
        fs::write(&temp_path, data)?;
        fs::rename(&temp_path, &path)?;

        Ok(true)
    }

    /// Sync get operation.
    pub fn get_sync(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.blob_path(hash);
        if path.exists() {
            Ok(Some(fs::read(&path)?))
        } else {
            Ok(None)
        }
    }

    /// Check if a hash exists.
    pub fn exists(&self, hash: &Hash) -> bool {
        self.blob_path(hash).exists()
    }

    /// Sync delete operation.
    pub fn delete_sync(&self, hash: &Hash) -> Result<bool, StoreError> {
        let path = self.blob_path(hash);
        if path.exists() {
            fs::remove_file(&path)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// List all hashes in the store.
    pub fn list(&self) -> Result<Vec<Hash>, StoreError> {
        let mut hashes = Vec::new();

        // Iterate over prefix directories (00-ff)
        let entries = match fs::read_dir(&self.base_path) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(hashes),
            Err(e) => return Err(e.into()),
        };

        for prefix_entry in entries {
            let prefix_entry = prefix_entry?;
            let prefix_path = prefix_entry.path();

            if !prefix_path.is_dir() {
                continue;
            }

            let prefix = match prefix_path.file_name().and_then(|n| n.to_str()) {
                Some(p) if p.len() == 2 => p.to_string(),
                _ => continue,
            };

            // Iterate over blobs in this prefix directory
            for blob_entry in fs::read_dir(&prefix_path)? {
                let blob_entry = blob_entry?;
                let rest = match blob_entry.file_name().to_str() {
                    Some(r) if r.len() == 62 => r.to_string(),
                    _ => continue,
                };

                // Reconstruct full hash hex
                let full_hex = format!("{}{}", prefix, rest);
                if let Ok(bytes) = hex::decode(&full_hex) {
                    if bytes.len() == 32 {
                        let mut hash = [0u8; 32];
                        hash.copy_from_slice(&bytes);
                        hashes.push(hash);
                    }
                }
            }
        }

        Ok(hashes)
    }

    /// Get storage statistics.
    pub fn stats(&self) -> Result<FsStats, StoreError> {
        let pins = self.pins.read().unwrap();
        let mut count = 0usize;
        let mut total_bytes = 0u64;
        let mut pinned_count = 0usize;
        let mut pinned_bytes = 0u64;

        let entries = match fs::read_dir(&self.base_path) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(FsStats {
                    count,
                    total_bytes,
                    pinned_count,
                    pinned_bytes,
                })
            }
            Err(e) => return Err(e.into()),
        };

        for prefix_entry in entries {
            let prefix_entry = prefix_entry?;
            let prefix_path = prefix_entry.path();

            if !prefix_path.is_dir() {
                continue;
            }

            let prefix = match prefix_path.file_name().and_then(|n| n.to_str()) {
                Some(p) if p.len() == 2 => p,
                _ => continue,
            };

            for blob_entry in fs::read_dir(&prefix_path)? {
                let blob_entry = blob_entry?;
                if blob_entry.path().is_file() {
                    let size = blob_entry.metadata()?.len();
                    count += 1;
                    total_bytes += size;

                    // Check if pinned
                    if let Some(rest) = blob_entry.file_name().to_str() {
                        let hex = format!("{}{}", prefix, rest);
                        if pins.get(&hex).copied().unwrap_or(0) > 0 {
                            pinned_count += 1;
                            pinned_bytes += size;
                        }
                    }
                }
            }
        }

        Ok(FsStats {
            count,
            total_bytes,
            pinned_count,
            pinned_bytes,
        })
    }

    /// Collect all blobs with their mtime and size for eviction
    fn collect_blobs_for_eviction(&self) -> Vec<(PathBuf, String, SystemTime, u64)> {
        let mut blobs = Vec::new();

        let entries = match fs::read_dir(&self.base_path) {
            Ok(e) => e,
            Err(_) => return blobs,
        };

        for prefix_entry in entries.flatten() {
            let prefix_path = prefix_entry.path();
            if !prefix_path.is_dir() {
                continue;
            }

            let prefix = match prefix_path.file_name().and_then(|n| n.to_str()) {
                Some(p) if p.len() == 2 => p.to_string(),
                _ => continue,
            };

            if let Ok(blob_entries) = fs::read_dir(&prefix_path) {
                for blob_entry in blob_entries.flatten() {
                    let path = blob_entry.path();
                    if !path.is_file() {
                        continue;
                    }

                    if let Ok(metadata) = blob_entry.metadata() {
                        let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                        let size = metadata.len();

                        if let Some(rest) = blob_entry.file_name().to_str() {
                            let hex = format!("{}{}", prefix, rest);
                            blobs.push((path, hex, mtime, size));
                        }
                    }
                }
            }
        }

        blobs
    }

    /// Evict unpinned blobs until storage is under target_bytes
    fn evict_to_target(&self, target_bytes: u64) -> u64 {
        let pins = self.pins.read().unwrap();

        // Collect all blobs
        let mut blobs = self.collect_blobs_for_eviction();

        // Filter to unpinned only
        blobs.retain(|(_, hex, _, _)| pins.get(hex).copied().unwrap_or(0) == 0);

        // Sort by mtime (oldest first)
        blobs.sort_by_key(|(_, _, mtime, _)| *mtime);

        drop(pins); // Release lock before deleting

        // Calculate current total
        let current_bytes: u64 = self
            .collect_blobs_for_eviction()
            .iter()
            .map(|(_, _, _, size)| *size)
            .sum();

        if current_bytes <= target_bytes {
            return 0;
        }

        let to_free = current_bytes - target_bytes;
        let mut freed = 0u64;

        for (path, _, _, size) in blobs {
            if freed >= to_free {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                freed += size;
            }
        }

        freed
    }
}

/// Storage statistics.
#[derive(Debug, Clone)]
pub struct FsStats {
    pub count: usize,
    pub total_bytes: u64,
    pub pinned_count: usize,
    pub pinned_bytes: u64,
}

#[async_trait]
impl Store for FsBlobStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.put_sync(hash, &data)
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.get_sync(hash)
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        Ok(self.exists(hash))
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        let hex = hex::encode(hash);
        // Remove pin entry if exists
        {
            let mut pins = self.pins.write().unwrap();
            pins.remove(&hex);
        }
        let _ = self.save_pins(); // Best effort
        self.delete_sync(hash)
    }

    fn set_max_bytes(&self, max: u64) {
        self.max_bytes.store(max, Ordering::Relaxed);
    }

    fn max_bytes(&self) -> Option<u64> {
        let max = self.max_bytes.load(Ordering::Relaxed);
        if max > 0 {
            Some(max)
        } else {
            None
        }
    }

    async fn stats(&self) -> StoreStats {
        match self.stats() {
            Ok(fs_stats) => StoreStats {
                count: fs_stats.count as u64,
                bytes: fs_stats.total_bytes,
                pinned_count: fs_stats.pinned_count as u64,
                pinned_bytes: fs_stats.pinned_bytes,
            },
            Err(_) => StoreStats::default(),
        }
    }

    async fn evict_if_needed(&self) -> Result<u64, StoreError> {
        let max = self.max_bytes.load(Ordering::Relaxed);
        if max == 0 {
            return Ok(0); // No limit set
        }

        let current = match self.stats() {
            Ok(s) => s.total_bytes,
            Err(_) => return Ok(0),
        };

        if current <= max {
            return Ok(0);
        }

        // Evict to 90% of max
        let target = max * 9 / 10;
        Ok(self.evict_to_target(target))
    }

    async fn pin(&self, hash: &Hash) -> Result<(), StoreError> {
        let hex = hex::encode(hash);
        {
            let mut pins = self.pins.write().unwrap();
            *pins.entry(hex).or_insert(0) += 1;
        }
        self.save_pins()
    }

    async fn unpin(&self, hash: &Hash) -> Result<(), StoreError> {
        let hex = hex::encode(hash);
        {
            let mut pins = self.pins.write().unwrap();
            if let Some(count) = pins.get_mut(&hex) {
                if *count > 0 {
                    *count -= 1;
                }
                if *count == 0 {
                    pins.remove(&hex);
                }
            }
        }
        self.save_pins()
    }

    fn pin_count(&self, hash: &Hash) -> u32 {
        let hex = hex::encode(hash);
        self.pins.read().unwrap().get(&hex).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_core::sha256;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_put_get() {
        let temp = TempDir::new().unwrap();
        let store = FsBlobStore::new(temp.path().join("blobs")).unwrap();

        let data = b"hello filesystem";
        let hash = sha256(data);
        store.put(hash, data.to_vec()).await.unwrap();

        assert!(store.has(&hash).await.unwrap());
        assert_eq!(store.get(&hash).await.unwrap(), Some(data.to_vec()));
    }

    #[tokio::test]
    async fn test_get_missing() {
        let temp = TempDir::new().unwrap();
        let store = FsBlobStore::new(temp.path().join("blobs")).unwrap();

        let hash = [0u8; 32];
        assert!(!store.has(&hash).await.unwrap());
        assert_eq!(store.get(&hash).await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_delete() {
        let temp = TempDir::new().unwrap();
        let store = FsBlobStore::new(temp.path().join("blobs")).unwrap();

        let data = b"delete me";
        let hash = sha256(data);
        store.put(hash, data.to_vec()).await.unwrap();
        assert!(store.has(&hash).await.unwrap());

        assert!(store.delete(&hash).await.unwrap());
        assert!(!store.has(&hash).await.unwrap());
        assert!(!store.delete(&hash).await.unwrap());
    }

    #[tokio::test]
    async fn test_deduplication() {
        let temp = TempDir::new().unwrap();
        let store = FsBlobStore::new(temp.path().join("blobs")).unwrap();

        let data = b"same content";
        let hash = sha256(data);

        // First put returns true (newly stored)
        assert!(store.put(hash, data.to_vec()).await.unwrap());
        // Second put returns false (already existed)
        assert!(!store.put(hash, data.to_vec()).await.unwrap());

        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_list() {
        let temp = TempDir::new().unwrap();
        let store = FsBlobStore::new(temp.path().join("blobs")).unwrap();

        let d1 = b"one";
        let d2 = b"two";
        let d3 = b"three";
        let h1 = sha256(d1);
        let h2 = sha256(d2);
        let h3 = sha256(d3);

        store.put(h1, d1.to_vec()).await.unwrap();
        store.put(h2, d2.to_vec()).await.unwrap();
        store.put(h3, d3.to_vec()).await.unwrap();

        let hashes = store.list().unwrap();
        assert_eq!(hashes.len(), 3);
        assert!(hashes.contains(&h1));
        assert!(hashes.contains(&h2));
        assert!(hashes.contains(&h3));
    }

    #[tokio::test]
    async fn test_stats() {
        let temp = TempDir::new().unwrap();
        let store = FsBlobStore::new(temp.path().join("blobs")).unwrap();

        let d1 = b"hello";
        let d2 = b"world";
        let h1 = sha256(d1);
        store.put(h1, d1.to_vec()).await.unwrap();
        store.put(sha256(d2), d2.to_vec()).await.unwrap();

        let stats = store.stats().unwrap();
        assert_eq!(stats.count, 2);
        assert_eq!(stats.total_bytes, 10);
        assert_eq!(stats.pinned_count, 0);
        assert_eq!(stats.pinned_bytes, 0);

        // Pin one item and check stats
        store.pin(&h1).await.unwrap();
        let stats = store.stats().unwrap();
        assert_eq!(stats.pinned_count, 1);
        assert_eq!(stats.pinned_bytes, 5);
    }

    #[tokio::test]
    async fn test_directory_structure() {
        let temp = TempDir::new().unwrap();
        let blobs_path = temp.path().join("blobs");
        let store = FsBlobStore::new(&blobs_path).unwrap();

        let data = b"test data";
        let hash = sha256(data);
        let hex = hex::encode(hash);

        store.put(hash, data.to_vec()).await.unwrap();

        // Verify the file exists at the correct path
        let prefix = &hex[..2];
        let rest = &hex[2..];
        let expected_path = blobs_path.join(prefix).join(rest);

        assert!(expected_path.exists(), "Blob should be at {:?}", expected_path);
        assert_eq!(fs::read(&expected_path).unwrap(), data);
    }

    #[test]
    fn test_blob_path_format() {
        let temp = TempDir::new().unwrap();
        let store = FsBlobStore::new(temp.path()).unwrap();

        // Hash: 0x00112233...
        let mut hash = [0u8; 32];
        hash[0] = 0x00;
        hash[1] = 0x11;
        hash[2] = 0x22;

        let path = store.blob_path(&hash);
        let path_str = path.to_string_lossy();

        // Should have "00" as directory prefix
        assert!(path_str.contains("/00/"), "Path should contain /00/ directory: {}", path_str);
        // File name should be remaining 62 chars
        assert!(path.file_name().unwrap().len() == 62);
    }

    #[tokio::test]
    async fn test_empty_store_stats() {
        let temp = TempDir::new().unwrap();
        let store = FsBlobStore::new(temp.path().join("blobs")).unwrap();

        let stats = store.stats().unwrap();
        assert_eq!(stats.count, 0);
        assert_eq!(stats.total_bytes, 0);
    }

    #[tokio::test]
    async fn test_empty_store_list() {
        let temp = TempDir::new().unwrap();
        let store = FsBlobStore::new(temp.path().join("blobs")).unwrap();

        let hashes = store.list().unwrap();
        assert!(hashes.is_empty());
    }

    #[tokio::test]
    async fn test_pin_and_unpin() {
        let temp = TempDir::new().unwrap();
        let store = FsBlobStore::new(temp.path().join("blobs")).unwrap();

        let data = b"pin me";
        let hash = sha256(data);
        store.put(hash, data.to_vec()).await.unwrap();

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

    #[tokio::test]
    async fn test_pin_ref_counting() {
        let temp = TempDir::new().unwrap();
        let store = FsBlobStore::new(temp.path().join("blobs")).unwrap();

        let data = b"multi pin";
        let hash = sha256(data);
        store.put(hash, data.to_vec()).await.unwrap();

        // Pin multiple times
        store.pin(&hash).await.unwrap();
        store.pin(&hash).await.unwrap();
        store.pin(&hash).await.unwrap();
        assert_eq!(store.pin_count(&hash), 3);

        // Unpin once
        store.unpin(&hash).await.unwrap();
        assert_eq!(store.pin_count(&hash), 2);
        assert!(store.is_pinned(&hash));

        // Unpin remaining
        store.unpin(&hash).await.unwrap();
        store.unpin(&hash).await.unwrap();
        assert_eq!(store.pin_count(&hash), 0);
    }

    #[tokio::test]
    async fn test_pins_persist_across_reload() {
        let temp = TempDir::new().unwrap();
        let blobs_path = temp.path().join("blobs");

        let data = b"persist me";
        let hash = sha256(data);

        // Create store and pin
        {
            let store = FsBlobStore::new(&blobs_path).unwrap();
            store.put(hash, data.to_vec()).await.unwrap();
            store.pin(&hash).await.unwrap();
            store.pin(&hash).await.unwrap();
            assert_eq!(store.pin_count(&hash), 2);
        }

        // Reload store
        {
            let store = FsBlobStore::new(&blobs_path).unwrap();
            assert_eq!(store.pin_count(&hash), 2);
            assert!(store.is_pinned(&hash));
        }
    }

    #[tokio::test]
    async fn test_max_bytes() {
        let temp = TempDir::new().unwrap();
        let store = FsBlobStore::new(temp.path().join("blobs")).unwrap();

        assert!(store.max_bytes().is_none());

        store.set_max_bytes(1000);
        assert_eq!(store.max_bytes(), Some(1000));

        store.set_max_bytes(0);
        assert!(store.max_bytes().is_none());
    }

    #[tokio::test]
    async fn test_with_max_bytes() {
        let temp = TempDir::new().unwrap();
        let store = FsBlobStore::with_max_bytes(temp.path().join("blobs"), 500).unwrap();
        assert_eq!(store.max_bytes(), Some(500));
    }

    #[tokio::test]
    async fn test_eviction_respects_pins() {
        let temp = TempDir::new().unwrap();
        // 20 byte limit
        let store = FsBlobStore::with_max_bytes(temp.path().join("blobs"), 20).unwrap();

        // Add items (5 bytes each = 15 total)
        let d1 = b"aaaaa"; // oldest - will be pinned
        let d2 = b"bbbbb";
        let d3 = b"ccccc";
        let h1 = sha256(d1);
        let h2 = sha256(d2);
        let h3 = sha256(d3);

        store.put(h1, d1.to_vec()).await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10)); // Ensure different mtime
        store.put(h2, d2.to_vec()).await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.put(h3, d3.to_vec()).await.unwrap();

        // Pin the oldest
        store.pin(&h1).await.unwrap();

        // Add more to exceed limit (15 + 5 = 20, at limit)
        let d4 = b"ddddd";
        let h4 = sha256(d4);
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.put(h4, d4.to_vec()).await.unwrap();

        // Add one more to exceed (20 + 5 = 25 > 20)
        let d5 = b"eeeee";
        let h5 = sha256(d5);
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.put(h5, d5.to_vec()).await.unwrap();

        // Evict
        let freed = store.evict_if_needed().await.unwrap();
        assert!(freed > 0, "Should have freed some bytes");

        // Pinned item should still exist
        assert!(store.has(&h1).await.unwrap(), "Pinned item should exist");
        // Oldest unpinned (h2) should be evicted
        assert!(!store.has(&h2).await.unwrap(), "Oldest unpinned should be evicted");
        // Newest should exist
        assert!(store.has(&h5).await.unwrap(), "Newest should exist");
    }

    #[tokio::test]
    async fn test_no_eviction_when_under_limit() {
        let temp = TempDir::new().unwrap();
        let store = FsBlobStore::with_max_bytes(temp.path().join("blobs"), 1000).unwrap();

        let data = b"small";
        let hash = sha256(data);
        store.put(hash, data.to_vec()).await.unwrap();

        let freed = store.evict_if_needed().await.unwrap();
        assert_eq!(freed, 0);
        assert!(store.has(&hash).await.unwrap());
    }

    #[tokio::test]
    async fn test_no_eviction_without_limit() {
        let temp = TempDir::new().unwrap();
        let store = FsBlobStore::new(temp.path().join("blobs")).unwrap();

        for i in 0..10u8 {
            let data = vec![i; 100];
            let hash = sha256(&data);
            store.put(hash, data).await.unwrap();
        }

        let freed = store.evict_if_needed().await.unwrap();
        assert_eq!(freed, 0);
        assert_eq!(store.list().unwrap().len(), 10);
    }

    #[tokio::test]
    async fn test_delete_removes_pin() {
        let temp = TempDir::new().unwrap();
        let store = FsBlobStore::new(temp.path().join("blobs")).unwrap();

        let data = b"delete pinned";
        let hash = sha256(data);
        store.put(hash, data.to_vec()).await.unwrap();
        store.pin(&hash).await.unwrap();
        assert!(store.is_pinned(&hash));

        store.delete(&hash).await.unwrap();
        assert_eq!(store.pin_count(&hash), 0);
    }
}
