//! Positive-only immutable read reuse for the append-only, single-writer catch-up run.
//! Writes still pass through the guarded store; misses and failures are never cached.
use std::{
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use async_trait::async_trait;
use hashtree_core::store::StoreStats;
use hashtree_core::{verify, Hash, Store, StoreError};
use lru::LruCache;

const MAX_ENTRIES: usize = 16_384;

struct Cache {
    entries: LruCache<Hash, Vec<u8>>,
    bytes: usize,
}

pub(super) struct CatchupReadCache<S: Store> {
    base: Arc<S>,
    maximum_bytes: usize,
    cache: Mutex<Cache>,
    hits: AtomicU64,
    misses: AtomicU64,
    write_calls: AtomicU64,
    submitted_write_bytes: AtomicU64,
}

impl<S: Store> CatchupReadCache<S> {
    pub(super) fn new(base: Arc<S>, maximum_bytes: usize) -> Self {
        Self {
            base,
            maximum_bytes,
            cache: Mutex::new(Cache {
                entries: LruCache::new(NonZeroUsize::new(MAX_ENTRIES).unwrap()),
                bytes: 0,
            }),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            write_calls: AtomicU64::new(0),
            submitted_write_bytes: AtomicU64::new(0),
        }
    }

    /// Cumulative counters for this process; payload bytes exclude bounded LRU metadata.
    pub(super) fn read_stats(&self) -> (u64, u64, usize, usize) {
        let cache = self.cache.lock().unwrap();
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
            cache.bytes,
            cache.entries.len(),
        )
    }

    /// Attempted backing writes, not inserted payload or physical disk bytes.
    pub(super) fn write_stats(&self) -> (u64, u64) {
        (
            self.write_calls.load(Ordering::Relaxed),
            self.submitted_write_bytes.load(Ordering::Relaxed),
        )
    }

    fn record_write(&self, bytes: usize) {
        self.write_calls.fetch_add(1, Ordering::Relaxed);
        self.submitted_write_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn forget(&self, hashes: &[Hash]) {
        let mut cache = self.cache.lock().unwrap();
        for hash in hashes {
            if let Some(bytes) = cache.entries.pop(hash) {
                cache.bytes -= bytes.len();
            }
        }
    }
}

#[async_trait]
impl<S: Store> Store for CatchupReadCache<S> {
    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        if self.maximum_bytes > 0 {
            let mut cache = self.cache.lock().unwrap();
            if let Some(bytes) = cache.entries.get(hash) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(Some(bytes.clone()));
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        // Never hold the cache lock across disk or network I/O. This wrapper is
        // private to the append-only catch-up writer: there is no concurrent GC.
        let result = self.base.get(hash).await?;
        if let Some(bytes) = &result {
            if self.maximum_bytes > 0 && bytes.len() <= self.maximum_bytes && verify(hash, bytes) {
                let mut cache = self.cache.lock().unwrap();
                if let Some(previous) = cache.entries.pop(hash) {
                    cache.bytes -= previous.len();
                }
                while cache.bytes + bytes.len() > self.maximum_bytes
                    || cache.entries.len() == MAX_ENTRIES
                {
                    let (_, removed) = cache.entries.pop_lru().expect("nonempty full cache");
                    cache.bytes -= removed.len();
                }
                cache.bytes += bytes.len();
                cache.entries.put(*hash, bytes.clone());
            }
        }
        Ok(result)
    }
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.record_write(data.len());
        self.forget(&[hash]);
        self.base.put(hash, data).await
    }
    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.record_write(items.iter().map(|(_, data)| data.len()).sum());
        self.forget(&items.iter().map(|(hash, _)| *hash).collect::<Vec<_>>());
        self.base.put_many(items).await
    }
    async fn put_many_optimistic(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.record_write(items.iter().map(|(_, data)| data.len()).sum());
        self.forget(&items.iter().map(|(hash, _)| *hash).collect::<Vec<_>>());
        self.base.put_many_optimistic(items).await
    }
    async fn flush_pending(&self) -> Result<usize, StoreError> {
        self.base.flush_pending().await
    }
    async fn get_range(
        &self,
        hash: &Hash,
        start: u64,
        end: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.base.get_range(hash, start, end).await
    }
    async fn blob_size(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        self.base.blob_size(hash).await
    }
    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.base.has(hash).await
    }
    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.forget(&[*hash]);
        self.base.delete(hash).await
    }
    async fn delete_many(&self, hashes: Vec<Hash>) -> Result<usize, StoreError> {
        self.forget(&hashes);
        self.base.delete_many(hashes).await
    }
    fn set_max_bytes(&self, max: u64) {
        self.base.set_max_bytes(max);
    }
    fn max_bytes(&self) -> Option<u64> {
        self.base.max_bytes()
    }
    async fn stats(&self) -> StoreStats {
        self.base.stats().await
    }
    async fn evict_if_needed(&self) -> Result<u64, StoreError> {
        {
            let mut cache = self.cache.lock().unwrap();
            cache.entries.clear();
            cache.bytes = 0;
        }
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

#[cfg(test)]
mod tests;
