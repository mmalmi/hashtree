//! Content-addressed key-value store interfaces and implementations

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::types::{to_hex, Hash};

/// Return a byte range from an in-memory blob.
///
/// `end_inclusive` follows HTTP/storage range conventions. Out-of-bounds ranges
/// return an empty or clamped slice instead of an error.
pub fn slice_blob_range(
    data: &[u8],
    start: u64,
    end_inclusive: u64,
) -> Result<Vec<u8>, StoreError> {
    if data.is_empty() || end_inclusive < start {
        return Ok(Vec::new());
    }

    let len = data.len() as u64;
    if start >= len {
        return Ok(Vec::new());
    }

    let actual_end = end_inclusive.min(len - 1);
    let start = usize::try_from(start)
        .map_err(|_| StoreError::Other("blob range start is too large".to_string()))?;
    let end_exclusive = usize::try_from(actual_end.saturating_add(1))
        .map_err(|_| StoreError::Other("blob range end is too large".to_string()))?;

    Ok(data[start..end_exclusive].to_vec())
}

/// Storage statistics
#[derive(Debug, Clone, Default)]
pub struct StoreStats {
    /// Number of items in store
    pub count: u64,
    /// Total bytes stored
    pub bytes: u64,
    /// Number of pinned items
    pub pinned_count: u64,
    /// Bytes used by pinned items
    pub pinned_bytes: u64,
}

/// Result of a batch insert.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PutManyReport {
    /// Number of candidate items passed by the caller, including duplicates.
    pub total: usize,
    /// Number of blobs newly inserted into the store.
    pub inserted: usize,
    /// Logical bytes newly inserted into the store.
    pub inserted_bytes: u64,
    /// Hashes that were newly inserted, in insertion order.
    pub inserted_hashes: Vec<Hash>,
}

/// Content-addressed key-value store interface
#[async_trait]
pub trait Store: Send + Sync {
    /// Store data by its hash
    /// Returns true if newly stored, false if already existed
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError>;

    /// Store multiple blobs.
    /// Returns the number of newly stored items.
    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        let mut inserted = 0usize;
        for (hash, data) in items {
            if self.put(hash, data).await? {
                inserted += 1;
            }
        }
        Ok(inserted)
    }

    /// Store a locally generated content-addressed batch without requiring an
    /// implementation to reread bytes for hashes it already tracks.
    ///
    /// Callers must provide bytes matching every hash. The default keeps the
    /// ordinary verified batch behavior; stores with an authenticated local
    /// location catalog may override this to skip known hashes cheaply.
    async fn put_many_optimistic(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.put_many(items).await
    }

    /// Retrieve data by hash
    /// Returns data or None if not found
    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError>;

    /// Retrieve a byte range from a blob by hash.
    ///
    /// `end_inclusive` is inclusive, matching HTTP range requests and the sync
    /// filesystem/LMDB storage primitives. Implementations may override this to
    /// avoid loading whole blobs.
    async fn get_range(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(data) = self.get(hash).await? else {
            return Ok(None);
        };
        Ok(Some(slice_blob_range(&data, start, end_inclusive)?))
    }

    /// Return the stored blob size in bytes.
    ///
    /// The default implementation reads the blob. Backends with metadata should
    /// override this to answer without loading payload bytes.
    async fn blob_size(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        Ok(self.get(hash).await?.map(|data| data.len() as u64))
    }

    /// Check if hash exists
    async fn has(&self, hash: &Hash) -> Result<bool, StoreError>;

    /// Delete by hash
    /// Returns true if deleted, false if didn't exist
    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError>;

    // ========================================================================
    // Optional: Storage limits and eviction (default no-op implementations)
    // ========================================================================

    /// Set maximum storage size in bytes. 0 = unlimited.
    fn set_max_bytes(&self, _max: u64) {}

    /// Get maximum storage size. None = unlimited.
    fn max_bytes(&self) -> Option<u64> {
        None
    }

    /// Get storage statistics
    async fn stats(&self) -> StoreStats {
        StoreStats::default()
    }

    /// Evict unpinned items if over storage limit.
    /// Returns number of bytes freed.
    async fn evict_if_needed(&self) -> Result<u64, StoreError> {
        Ok(0)
    }

    // ========================================================================
    // Optional: Pinning (default no-op implementations)
    // ========================================================================

    /// Pin a hash (increment ref count). Pinned items are not evicted.
    async fn pin(&self, _hash: &Hash) -> Result<(), StoreError> {
        Ok(())
    }

    /// Unpin a hash (decrement ref count). Item can be evicted when count reaches 0.
    async fn unpin(&self, _hash: &Hash) -> Result<(), StoreError> {
        Ok(())
    }

    /// Get pin count for a hash. 0 = not pinned.
    fn pin_count(&self, _hash: &Hash) -> u32 {
        0
    }

    /// Check if hash is pinned (pin count > 0)
    fn is_pinned(&self, hash: &Hash) -> bool {
        self.pin_count(hash) > 0
    }
}

/// Store error type
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Store error: {0}")]
    Other(String),
}

#[derive(Debug, Default)]
struct BufferedStoreInner {
    pending: HashMap<Hash, Vec<u8>>,
    order: Vec<Hash>,
}

#[derive(Debug, Clone, Copy)]
struct BufferedStoreOptions {
    check_base_on_put: bool,
}

impl Default for BufferedStoreOptions {
    fn default() -> Self {
        Self {
            check_base_on_put: true,
        }
    }
}

/// Buffered overlay store that keeps writes in memory until flushed.
#[derive(Debug, Clone)]
pub struct BufferedStore<S: Store> {
    base: Arc<S>,
    inner: Arc<RwLock<BufferedStoreInner>>,
    options: BufferedStoreOptions,
}

impl<S: Store> BufferedStore<S> {
    pub fn new(base: Arc<S>) -> Self {
        Self::with_options(base, BufferedStoreOptions::default())
    }

    pub fn new_optimistic(base: Arc<S>) -> Self {
        Self::with_options(
            base,
            BufferedStoreOptions {
                check_base_on_put: false,
            },
        )
    }

    fn with_options(base: Arc<S>, options: BufferedStoreOptions) -> Self {
        Self {
            base,
            inner: Arc::new(RwLock::new(BufferedStoreInner::default())),
            options,
        }
    }

    pub async fn flush(&self) -> Result<usize, StoreError> {
        let items = {
            let mut inner = self.inner.write().unwrap();
            if inner.order.is_empty() {
                return Ok(0);
            }

            let order = std::mem::take(&mut inner.order);
            let mut items = Vec::with_capacity(order.len());
            for hash in order {
                if let Some(data) = inner.pending.remove(&hash) {
                    items.push((hash, data));
                }
            }
            items
        };

        if self.options.check_base_on_put {
            self.base.put_many(items).await
        } else {
            self.base.put_many_optimistic(items).await
        }
    }
}

#[async_trait]
impl<S: Store> Store for BufferedStore<S> {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        {
            let inner = self.inner.read().unwrap();
            if inner.pending.contains_key(&hash) {
                return Ok(false);
            }
        }

        if self.options.check_base_on_put && self.base.has(&hash).await? {
            return Ok(false);
        }

        let mut inner = self.inner.write().unwrap();
        if inner.pending.contains_key(&hash) {
            return Ok(false);
        }
        inner.order.push(hash);
        inner.pending.insert(hash, data);
        Ok(true)
    }

    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        let mut inserted = 0usize;
        for (hash, data) in items {
            if self.put(hash, data).await? {
                inserted += 1;
            }
        }
        Ok(inserted)
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        if let Some(data) = self.inner.read().unwrap().pending.get(hash).cloned() {
            return Ok(Some(data));
        }
        self.base.get(hash).await
    }

    async fn get_range(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let pending = {
            let inner = self.inner.read().unwrap();
            inner
                .pending
                .get(hash)
                .map(|data| slice_blob_range(data, start, end_inclusive))
                .transpose()?
        };
        if pending.is_some() {
            return Ok(pending);
        }
        self.base.get_range(hash, start, end_inclusive).await
    }

    async fn blob_size(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        if let Some(size) = self
            .inner
            .read()
            .unwrap()
            .pending
            .get(hash)
            .map(|data| data.len() as u64)
        {
            return Ok(Some(size));
        }
        self.base.blob_size(hash).await
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        if self.inner.read().unwrap().pending.contains_key(hash) {
            return Ok(true);
        }
        self.base.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        let removed = {
            let mut inner = self.inner.write().unwrap();
            let removed = inner.pending.remove(hash).is_some();
            if removed {
                inner.order.retain(|queued| queued != hash);
            }
            removed
        };

        if removed {
            return Ok(true);
        }

        self.base.delete(hash).await
    }

    async fn stats(&self) -> StoreStats {
        let mut stats = self.base.stats().await;
        let pending_bytes = self
            .inner
            .read()
            .unwrap()
            .pending
            .values()
            .map(|data| data.len() as u64)
            .sum::<u64>();
        stats.count += self.inner.read().unwrap().pending.len() as u64;
        stats.bytes += pending_bytes;
        stats
    }

    async fn evict_if_needed(&self) -> Result<u64, StoreError> {
        self.base.evict_if_needed().await
    }

    async fn pin(&self, hash: &Hash) -> Result<(), StoreError> {
        self.base.pin(hash).await
    }

    async fn unpin(&self, hash: &Hash) -> Result<(), StoreError> {
        self.base.unpin(hash).await
    }

    fn pin_count(&self, hash: &Hash) -> u32 {
        self.base.pin_count(hash)
    }
}

/// Entry in the memory store with metadata for LRU
#[derive(Debug, Clone)]
struct MemoryEntry {
    data: Vec<u8>,
    /// Insertion order for LRU (lower = older)
    order: u64,
}

/// Internal state for MemoryStore
#[derive(Debug, Default)]
struct MemoryStoreInner {
    data: HashMap<String, MemoryEntry>,
    pins: HashMap<String, u32>,
    next_order: u64,
    max_bytes: Option<u64>,
}

/// In-memory content-addressed store with LRU eviction and pinning
#[derive(Debug, Clone, Default)]
pub struct MemoryStore {
    inner: Arc<RwLock<MemoryStoreInner>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(MemoryStoreInner::default())),
        }
    }

    /// Create a new store with a maximum size limit
    pub fn with_max_bytes(max_bytes: u64) -> Self {
        Self {
            inner: Arc::new(RwLock::new(MemoryStoreInner {
                max_bytes: if max_bytes > 0 { Some(max_bytes) } else { None },
                ..Default::default()
            })),
        }
    }

    /// Get number of stored items
    pub fn size(&self) -> usize {
        self.inner.read().unwrap().data.len()
    }

    /// Get total bytes stored
    pub fn total_bytes(&self) -> usize {
        self.inner
            .read()
            .unwrap()
            .data
            .values()
            .map(|e| e.data.len())
            .sum()
    }

    /// Clear all data (but not pins)
    pub fn clear(&self) {
        self.inner.write().unwrap().data.clear();
    }

    /// List all hashes
    pub fn keys(&self) -> Vec<Hash> {
        self.inner
            .read()
            .unwrap()
            .data
            .keys()
            .filter_map(|hex| {
                let bytes = hex::decode(hex).ok()?;
                if bytes.len() != 32 {
                    return None;
                }
                let mut hash = [0u8; 32];
                hash.copy_from_slice(&bytes);
                Some(hash)
            })
            .collect()
    }

    /// Evict oldest unpinned entries until under target bytes
    fn evict_to_target(&self, target_bytes: u64) -> u64 {
        let mut inner = self.inner.write().unwrap();

        let current_bytes: u64 = inner.data.values().map(|e| e.data.len() as u64).sum();
        if current_bytes <= target_bytes {
            return 0;
        }

        // Collect unpinned entries sorted by order (oldest first)
        let mut unpinned: Vec<(String, u64, u64)> = inner
            .data
            .iter()
            .filter(|(key, _)| inner.pins.get(*key).copied().unwrap_or(0) == 0)
            .map(|(key, entry)| (key.clone(), entry.order, entry.data.len() as u64))
            .collect();

        unpinned.sort_by_key(|(_, order, _)| *order);

        let mut freed = 0u64;
        let to_free = current_bytes - target_bytes;

        for (key, _, size) in unpinned {
            if freed >= to_free {
                break;
            }
            inner.data.remove(&key);
            freed += size;
        }

        freed
    }
}

#[async_trait]
impl Store for MemoryStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        let key = to_hex(&hash);
        let mut inner = self.inner.write().unwrap();
        if inner.data.contains_key(&key) {
            return Ok(false);
        }
        let order = inner.next_order;
        inner.next_order += 1;
        inner.data.insert(key, MemoryEntry { data, order });
        Ok(true)
    }

    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        let mut inserted = 0usize;
        let mut inner = self.inner.write().unwrap();
        for (hash, data) in items {
            let key = to_hex(&hash);
            if inner.data.contains_key(&key) {
                continue;
            }
            let order = inner.next_order;
            inner.next_order += 1;
            inner.data.insert(key, MemoryEntry { data, order });
            inserted += 1;
        }
        Ok(inserted)
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let key = to_hex(hash);
        let inner = self.inner.read().unwrap();
        Ok(inner.data.get(&key).map(|e| e.data.clone()))
    }

    async fn get_range(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let key = to_hex(hash);
        let inner = self.inner.read().unwrap();
        inner
            .data
            .get(&key)
            .map(|entry| slice_blob_range(&entry.data, start, end_inclusive))
            .transpose()
    }

    async fn blob_size(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        let key = to_hex(hash);
        let inner = self.inner.read().unwrap();
        Ok(inner.data.get(&key).map(|entry| entry.data.len() as u64))
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        let key = to_hex(hash);
        Ok(self.inner.read().unwrap().data.contains_key(&key))
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        let key = to_hex(hash);
        let mut inner = self.inner.write().unwrap();
        // Also remove pin entry if exists
        inner.pins.remove(&key);
        Ok(inner.data.remove(&key).is_some())
    }

    fn set_max_bytes(&self, max: u64) {
        self.inner.write().unwrap().max_bytes = if max > 0 { Some(max) } else { None };
    }

    fn max_bytes(&self) -> Option<u64> {
        self.inner.read().unwrap().max_bytes
    }

    async fn stats(&self) -> StoreStats {
        let inner = self.inner.read().unwrap();
        let mut count = 0u64;
        let mut bytes = 0u64;
        let mut pinned_count = 0u64;
        let mut pinned_bytes = 0u64;

        for (key, entry) in &inner.data {
            count += 1;
            bytes += entry.data.len() as u64;
            if inner.pins.get(key).copied().unwrap_or(0) > 0 {
                pinned_count += 1;
                pinned_bytes += entry.data.len() as u64;
            }
        }

        StoreStats {
            count,
            bytes,
            pinned_count,
            pinned_bytes,
        }
    }

    async fn evict_if_needed(&self) -> Result<u64, StoreError> {
        let max = match self.inner.read().unwrap().max_bytes {
            Some(m) => m,
            None => return Ok(0), // No limit set
        };

        let current: u64 = self
            .inner
            .read()
            .unwrap()
            .data
            .values()
            .map(|e| e.data.len() as u64)
            .sum();

        if current <= max {
            return Ok(0);
        }

        // Evict to 90% of max to avoid frequent evictions
        let target = max * 9 / 10;
        Ok(self.evict_to_target(target))
    }

    async fn pin(&self, hash: &Hash) -> Result<(), StoreError> {
        let key = to_hex(hash);
        let mut inner = self.inner.write().unwrap();
        *inner.pins.entry(key).or_insert(0) += 1;
        Ok(())
    }

    async fn unpin(&self, hash: &Hash) -> Result<(), StoreError> {
        let key = to_hex(hash);
        let mut inner = self.inner.write().unwrap();
        if let Some(count) = inner.pins.get_mut(&key) {
            if *count > 0 {
                *count -= 1;
            }
            if *count == 0 {
                inner.pins.remove(&key);
            }
        }
        Ok(())
    }

    fn pin_count(&self, hash: &Hash) -> u32 {
        let key = to_hex(hash);
        self.inner
            .read()
            .unwrap()
            .pins
            .get(&key)
            .copied()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::sha256;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct OptimisticBatchStore {
        inner: MemoryStore,
        regular_batches: AtomicUsize,
        optimistic_batches: AtomicUsize,
    }

    #[async_trait]
    impl Store for OptimisticBatchStore {
        async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
            self.inner.put(hash, data).await
        }

        async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
            self.regular_batches.fetch_add(1, Ordering::Relaxed);
            self.inner.put_many(items).await
        }

        async fn put_many_optimistic(
            &self,
            items: Vec<(Hash, Vec<u8>)>,
        ) -> Result<usize, StoreError> {
            self.optimistic_batches.fetch_add(1, Ordering::Relaxed);
            self.inner.put_many(items).await
        }

        async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
            self.inner.get(hash).await
        }

        async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
            self.inner.has(hash).await
        }

        async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
            self.inner.delete(hash).await
        }
    }

    #[tokio::test]
    async fn test_put_returns_true_for_new() {
        let store = MemoryStore::new();
        let data = vec![1u8, 2, 3];
        let hash = sha256(&data);

        let result = store.put(hash, data).await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn test_put_returns_false_for_duplicate() {
        let store = MemoryStore::new();
        let data = vec![1u8, 2, 3];
        let hash = sha256(&data);

        store.put(hash, data.clone()).await.unwrap();
        let result = store.put(hash, data).await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn test_put_many_counts_only_new_items() {
        let store = MemoryStore::new();
        let data1 = vec![1u8, 2, 3];
        let data2 = vec![4u8, 5, 6];
        let hash1 = sha256(&data1);
        let hash2 = sha256(&data2);

        store.put(hash1, data1.clone()).await.unwrap();
        let inserted = store
            .put_many(vec![(hash1, data1), (hash2, data2.clone())])
            .await
            .unwrap();

        assert_eq!(inserted, 1);
        assert_eq!(store.get(&hash2).await.unwrap(), Some(data2));
    }

    #[tokio::test]
    async fn test_buffered_store_flushes_pending_writes() {
        let base = std::sync::Arc::new(MemoryStore::new());
        let buffered = BufferedStore::new(std::sync::Arc::clone(&base));
        let data = vec![9u8, 8, 7];
        let hash = sha256(&data);

        assert!(buffered.put(hash, data.clone()).await.unwrap());
        assert_eq!(buffered.get(&hash).await.unwrap(), Some(data.clone()));
        assert_eq!(base.get(&hash).await.unwrap(), None);

        let flushed = buffered.flush().await.unwrap();

        assert_eq!(flushed, 1);
        assert_eq!(base.get(&hash).await.unwrap(), Some(data));
    }

    #[tokio::test]
    async fn test_optimistic_buffered_store_avoids_base_probe_but_preserves_contents() {
        let base = std::sync::Arc::new(MemoryStore::new());
        let buffered = BufferedStore::new_optimistic(std::sync::Arc::clone(&base));
        let data = vec![4u8, 5, 6];
        let hash = sha256(&data);

        base.put(hash, data.clone()).await.unwrap();

        assert!(buffered.put(hash, data.clone()).await.unwrap());
        assert_eq!(buffered.get(&hash).await.unwrap(), Some(data.clone()));

        let flushed = buffered.flush().await.unwrap();

        assert_eq!(flushed, 0);
        assert_eq!(base.get(&hash).await.unwrap(), Some(data));
    }

    #[tokio::test]
    async fn test_optimistic_buffered_store_uses_idempotent_batch_flush() {
        let base = std::sync::Arc::new(OptimisticBatchStore::default());
        let buffered = BufferedStore::new_optimistic(std::sync::Arc::clone(&base));
        let data = vec![7u8; 1024];
        let hash = sha256(&data);

        base.inner.put(hash, data.clone()).await.unwrap();
        assert!(buffered.put(hash, data).await.unwrap());
        assert_eq!(buffered.flush().await.unwrap(), 0);

        assert_eq!(base.regular_batches.load(Ordering::Relaxed), 0);
        assert_eq!(base.optimistic_batches.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_get_returns_data() {
        let store = MemoryStore::new();
        let data = vec![1u8, 2, 3];
        let hash = sha256(&data);

        store.put(hash, data.clone()).await.unwrap();
        let result = store.get(&hash).await.unwrap();

        assert_eq!(result, Some(data));
    }

    #[tokio::test]
    async fn test_get_returns_none_for_missing() {
        let store = MemoryStore::new();
        let hash = [0u8; 32];

        let result = store.get(&hash).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_has_returns_true() {
        let store = MemoryStore::new();
        let data = vec![1u8, 2, 3];
        let hash = sha256(&data);

        store.put(hash, data).await.unwrap();
        assert!(store.has(&hash).await.unwrap());
    }

    #[tokio::test]
    async fn test_has_returns_false() {
        let store = MemoryStore::new();
        let hash = [0u8; 32];

        assert!(!store.has(&hash).await.unwrap());
    }

    #[tokio::test]
    async fn test_delete_returns_true() {
        let store = MemoryStore::new();
        let data = vec![1u8, 2, 3];
        let hash = sha256(&data);

        store.put(hash, data).await.unwrap();
        let result = store.delete(&hash).await.unwrap();

        assert!(result);
        assert!(!store.has(&hash).await.unwrap());
    }

    #[tokio::test]
    async fn test_delete_returns_false() {
        let store = MemoryStore::new();
        let hash = [0u8; 32];

        let result = store.delete(&hash).await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn test_size() {
        let store = MemoryStore::new();
        assert_eq!(store.size(), 0);

        let data1 = vec![1u8];
        let data2 = vec![2u8];
        let hash1 = sha256(&data1);
        let hash2 = sha256(&data2);

        store.put(hash1, data1).await.unwrap();
        store.put(hash2, data2).await.unwrap();

        assert_eq!(store.size(), 2);
    }

    #[tokio::test]
    async fn test_total_bytes() {
        let store = MemoryStore::new();
        assert_eq!(store.total_bytes(), 0);

        let data1 = vec![1u8, 2, 3];
        let data2 = vec![4u8, 5];
        let hash1 = sha256(&data1);
        let hash2 = sha256(&data2);

        store.put(hash1, data1).await.unwrap();
        store.put(hash2, data2).await.unwrap();

        assert_eq!(store.total_bytes(), 5);
    }

    #[tokio::test]
    async fn test_clear() {
        let store = MemoryStore::new();
        let data = vec![1u8, 2, 3];
        let hash = sha256(&data);

        store.put(hash, data).await.unwrap();
        store.clear();

        assert_eq!(store.size(), 0);
        assert!(!store.has(&hash).await.unwrap());
    }

    #[tokio::test]
    async fn test_keys() {
        let store = MemoryStore::new();
        assert!(store.keys().is_empty());

        let data1 = vec![1u8];
        let data2 = vec![2u8];
        let hash1 = sha256(&data1);
        let hash2 = sha256(&data2);

        store.put(hash1, data1).await.unwrap();
        store.put(hash2, data2).await.unwrap();

        let keys = store.keys();
        assert_eq!(keys.len(), 2);

        let mut hex_keys: Vec<_> = keys.iter().map(to_hex).collect();
        hex_keys.sort();
        let mut expected: Vec<_> = vec![to_hex(&hash1), to_hex(&hash2)];
        expected.sort();
        assert_eq!(hex_keys, expected);
    }

    #[tokio::test]
    async fn test_pin_and_unpin() {
        let store = MemoryStore::new();
        let data = vec![1u8, 2, 3];
        let hash = sha256(&data);

        store.put(hash, data).await.unwrap();

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
    async fn test_pin_count_ref_counting() {
        let store = MemoryStore::new();
        let data = vec![1u8, 2, 3];
        let hash = sha256(&data);

        store.put(hash, data).await.unwrap();

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
        assert!(!store.is_pinned(&hash));

        // Extra unpin shouldn't go negative
        store.unpin(&hash).await.unwrap();
        assert_eq!(store.pin_count(&hash), 0);
    }

    #[tokio::test]
    async fn test_stats() {
        let store = MemoryStore::new();

        let data1 = vec![1u8, 2, 3]; // 3 bytes
        let data2 = vec![4u8, 5]; // 2 bytes
        let hash1 = sha256(&data1);
        let hash2 = sha256(&data2);

        store.put(hash1, data1).await.unwrap();
        store.put(hash2, data2).await.unwrap();

        // Pin one item
        store.pin(&hash1).await.unwrap();

        let stats = store.stats().await;
        assert_eq!(stats.count, 2);
        assert_eq!(stats.bytes, 5);
        assert_eq!(stats.pinned_count, 1);
        assert_eq!(stats.pinned_bytes, 3);
    }

    #[tokio::test]
    async fn test_max_bytes() {
        let store = MemoryStore::new();
        assert!(store.max_bytes().is_none());

        store.set_max_bytes(1000);
        assert_eq!(store.max_bytes(), Some(1000));

        // 0 means unlimited
        store.set_max_bytes(0);
        assert!(store.max_bytes().is_none());
    }

    #[tokio::test]
    async fn test_with_max_bytes() {
        let store = MemoryStore::with_max_bytes(500);
        assert_eq!(store.max_bytes(), Some(500));

        let store_unlimited = MemoryStore::with_max_bytes(0);
        assert!(store_unlimited.max_bytes().is_none());
    }

    #[tokio::test]
    async fn test_eviction_respects_pins() {
        // Store with 10 byte limit
        let store = MemoryStore::with_max_bytes(10);

        // Insert 3 items: 3 + 3 + 3 = 9 bytes
        let data1 = vec![1u8, 1, 1]; // oldest
        let data2 = vec![2u8, 2, 2];
        let data3 = vec![3u8, 3, 3]; // newest
        let hash1 = sha256(&data1);
        let hash2 = sha256(&data2);
        let hash3 = sha256(&data3);

        store.put(hash1, data1).await.unwrap();
        store.put(hash2, data2).await.unwrap();
        store.put(hash3, data3).await.unwrap();

        // Pin the oldest item
        store.pin(&hash1).await.unwrap();

        // Add more data to exceed limit: 9 + 3 = 12 bytes > 10
        let data4 = vec![4u8, 4, 4];
        let hash4 = sha256(&data4);
        store.put(hash4, data4).await.unwrap();

        // Evict - should remove hash2 (oldest unpinned)
        let freed = store.evict_if_needed().await.unwrap();
        assert!(freed > 0);

        // hash1 should still exist (pinned)
        assert!(store.has(&hash1).await.unwrap());
        // hash2 should be gone (oldest unpinned)
        assert!(!store.has(&hash2).await.unwrap());
        // hash3 and hash4 should exist
        assert!(store.has(&hash3).await.unwrap());
        assert!(store.has(&hash4).await.unwrap());
    }

    #[tokio::test]
    async fn test_eviction_lru_order() {
        // Store with 15 byte limit
        let store = MemoryStore::with_max_bytes(15);

        // Insert items in order (oldest first)
        let data1 = vec![1u8; 5]; // oldest
        let data2 = vec![2u8; 5];
        let data3 = vec![3u8; 5];
        let data4 = vec![4u8; 5]; // newest
        let hash1 = sha256(&data1);
        let hash2 = sha256(&data2);
        let hash3 = sha256(&data3);
        let hash4 = sha256(&data4);

        store.put(hash1, data1).await.unwrap();
        store.put(hash2, data2).await.unwrap();
        store.put(hash3, data3).await.unwrap();
        store.put(hash4, data4).await.unwrap();

        // Now at 20 bytes, limit is 15
        assert_eq!(store.total_bytes(), 20);

        // Evict - should remove oldest items first
        let freed = store.evict_if_needed().await.unwrap();
        assert!(freed >= 5); // At least one item evicted

        // Oldest should be gone
        assert!(!store.has(&hash1).await.unwrap());
        // Newest should still exist
        assert!(store.has(&hash4).await.unwrap());
    }

    #[tokio::test]
    async fn test_no_eviction_when_under_limit() {
        let store = MemoryStore::with_max_bytes(100);

        let data = vec![1u8, 2, 3];
        let hash = sha256(&data);
        store.put(hash, data).await.unwrap();

        let freed = store.evict_if_needed().await.unwrap();
        assert_eq!(freed, 0);
        assert!(store.has(&hash).await.unwrap());
    }

    #[tokio::test]
    async fn test_no_eviction_without_limit() {
        let store = MemoryStore::new();

        // Add lots of data
        for i in 0..100u8 {
            let data = vec![i; 100];
            let hash = sha256(&data);
            store.put(hash, data).await.unwrap();
        }

        let freed = store.evict_if_needed().await.unwrap();
        assert_eq!(freed, 0);
        assert_eq!(store.size(), 100);
    }

    #[tokio::test]
    async fn test_delete_removes_pin() {
        let store = MemoryStore::new();
        let data = vec![1u8, 2, 3];
        let hash = sha256(&data);

        store.put(hash, data).await.unwrap();
        store.pin(&hash).await.unwrap();
        assert!(store.is_pinned(&hash));

        store.delete(&hash).await.unwrap();
        // Pin should be gone after delete
        assert_eq!(store.pin_count(&hash), 0);
    }
}
