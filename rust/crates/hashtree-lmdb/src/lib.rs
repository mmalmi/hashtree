//! LMDB-backed content-addressed blob storage.

use async_trait::async_trait;
use hashtree_core::store::{Store, StoreError, StoreStats};
use hashtree_core::types::Hash;
use heed::types::*;
use heed::{Database, EnvOpenOptions, Error as HeedError, MdbError, PutFlags};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// Re-export sha256 for convenience
pub use hashtree_core::hash::sha256 as compute_sha256;

#[cfg(target_pointer_width = "64")]
const DEFAULT_MAP_SIZE: usize = 10 * 1024 * 1024 * 1024;
#[cfg(target_pointer_width = "32")]
const DEFAULT_MAP_SIZE: usize = 1024 * 1024 * 1024;
const DEFAULT_MAX_READERS: u32 = 1024;
const DATABASE_COUNT: u32 = 5;
const REOPEN_HEADROOM_BYTES: u64 = 64 * 1024 * 1024;
const EVICTION_BATCH_TARGET_BYTES: u64 = 256 * 1024 * 1024;
const EVICTION_BATCH_MAX_ITEMS: usize = 4096;
const LEGACY_BLOB_META_BYTES: usize = 16;
const BLOB_META_BYTES: usize = 24;
const ORDER_KEY_BYTES: usize = 40;
const PIN_COUNT_BYTES: usize = 4;
const STORE_TOTALS_BYTES: usize = 32;
const STORE_TOTALS_KEY: &[u8] = b"totals";

#[derive(Debug, Clone, Copy)]
struct BlobMeta {
    order: u64,
    size: u64,
    last_accessed_at: u64,
}

#[derive(Debug, Default, Clone, Copy)]
struct StoreTotals {
    count: u64,
    total_bytes: u64,
    pinned_count: u64,
    pinned_bytes: u64,
}

/// LMDB-backed blob store implementing hashtree's Store trait.
pub struct LmdbBlobStore {
    env: heed::Env,
    /// Maps SHA256 hash (32 bytes) → blob data
    blobs: Database<Bytes, Bytes>,
    /// Maps SHA256 hash (32 bytes) → [order: u64][size: u64]
    metadata: Database<Bytes, Bytes>,
    /// Maps [order: u64][hash: 32 bytes] → ()
    eviction_order: Database<Bytes, Unit>,
    /// Maps SHA256 hash (32 bytes) → pin count (u32)
    pins: Database<Bytes, Bytes>,
    /// Small aggregate counters used by quota checks and stats.
    stats: Database<Bytes, Bytes>,
    max_bytes: AtomicU64,
    current_bytes: AtomicU64,
    next_order: AtomicU64,
}

impl LmdbBlobStore {
    /// Open or create an LMDB blob store at the given path.
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, StoreError> {
        Self::with_map_size(path, DEFAULT_MAP_SIZE)
    }

    /// Open or create with a maximum logical storage size.
    pub fn with_max_bytes<P: AsRef<Path>>(path: P, max_bytes: u64) -> Result<Self, StoreError> {
        let path_ref = path.as_ref();
        let existing_map_size = std::fs::metadata(path_ref.join("data.mdb"))
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let existing_headroom = if existing_map_size == 0 {
            0
        } else {
            existing_map_size
                .saturating_div(10)
                .max(REOPEN_HEADROOM_BYTES)
        };
        let requested_map_size = usize::try_from(Self::align_map_size_bytes(
            existing_map_size
                .saturating_add(existing_headroom)
                .max(max_bytes),
        ))
        .unwrap_or(usize::MAX)
        .max(DEFAULT_MAP_SIZE);
        let store = Self::with_map_size(path_ref, requested_map_size)?;
        store.max_bytes.store(max_bytes, Ordering::Relaxed);
        let current = store.current_bytes.load(Ordering::Relaxed);
        if max_bytes > 0 && current > max_bytes {
            let target = max_bytes.saturating_mul(9) / 10;
            store.evict_to_target(current, target)?;
        }
        Ok(store)
    }

    fn align_map_size_bytes(bytes: u64) -> u64 {
        if bytes == 0 {
            return 0;
        }
        let page_size = (page_size::get() as u64).max(4096);
        let remainder = bytes % page_size;
        if remainder == 0 {
            bytes
        } else {
            bytes.saturating_add(page_size - remainder)
        }
    }

    /// Open or create with custom map size.
    pub fn with_map_size<P: AsRef<Path>>(path: P, map_size: usize) -> Result<Self, StoreError> {
        let path_ref = path.as_ref();
        std::fs::create_dir_all(path_ref).map_err(StoreError::Io)?;
        let existing_map_size = std::fs::metadata(path_ref.join("data.mdb"))
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let existing_headroom = if existing_map_size == 0 {
            0
        } else {
            existing_map_size
                .saturating_div(10)
                .max(REOPEN_HEADROOM_BYTES)
        };
        let requested_map_size = u64::try_from(map_size).unwrap_or(u64::MAX);
        let map_size = usize::try_from(Self::align_map_size_bytes(
            requested_map_size.max(existing_map_size.saturating_add(existing_headroom)),
        ))
        .unwrap_or(usize::MAX);

        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(map_size)
                .max_dbs(DATABASE_COUNT)
                .max_readers(DEFAULT_MAX_READERS)
                .open(path_ref)
                .map_err(map_heed_error)?
        };
        let _ = env.clear_stale_readers();
        if env.info().map_size < map_size {
            unsafe { env.resize(map_size) }.map_err(map_heed_error)?;
        }

        let mut wtxn = env.write_txn().map_err(map_heed_error)?;
        let blobs = env
            .create_database(&mut wtxn, Some("blobs"))
            .map_err(map_heed_error)?;
        let metadata = env
            .create_database(&mut wtxn, Some("metadata"))
            .map_err(map_heed_error)?;
        let eviction_order = env
            .create_database(&mut wtxn, Some("eviction_order"))
            .map_err(map_heed_error)?;
        let pins = env
            .create_database(&mut wtxn, Some("pins"))
            .map_err(map_heed_error)?;
        let stats = env
            .create_database(&mut wtxn, Some("stats"))
            .map_err(map_heed_error)?;
        wtxn.commit().map_err(map_heed_error)?;

        let next_order = {
            let rtxn = env.read_txn().map_err(map_heed_error)?;
            let next = eviction_order
                .iter(&rtxn)
                .map_err(map_heed_error)?
                .last()
                .transpose()
                .map_err(map_heed_error)?
                .map(|(key, _)| {
                    Self::decode_order_from_order_key(key).map(|order| order.saturating_add(1))
                })
                .transpose()?
                .unwrap_or(0);
            next
        };
        let totals = Self::load_or_rebuild_totals(&env, metadata, pins, stats)?;

        Ok(Self {
            env,
            blobs,
            metadata,
            eviction_order,
            pins,
            stats,
            max_bytes: AtomicU64::new(0),
            current_bytes: AtomicU64::new(totals.total_bytes),
            next_order: AtomicU64::new(next_order),
        })
    }

    fn load_or_rebuild_totals(
        env: &heed::Env,
        metadata: Database<Bytes, Bytes>,
        pins: Database<Bytes, Bytes>,
        stats: Database<Bytes, Bytes>,
    ) -> Result<StoreTotals, StoreError> {
        {
            let rtxn = env.read_txn().map_err(map_heed_error)?;
            if let Some(totals) = Self::read_store_totals(stats, &rtxn)? {
                return Ok(totals);
            }
        }

        let mut totals = StoreTotals::default();
        {
            let rtxn = env.read_txn().map_err(map_heed_error)?;
            for item in metadata.iter(&rtxn).map_err(map_heed_error)? {
                let (_, bytes) = item.map_err(map_heed_error)?;
                let meta = Self::decode_blob_meta(bytes)?;
                totals.count = totals.count.saturating_add(1);
                totals.total_bytes = totals.total_bytes.saturating_add(meta.size);
            }
            for item in pins.iter(&rtxn).map_err(map_heed_error)? {
                let (hash, pin_bytes) = item.map_err(map_heed_error)?;
                let count = Self::decode_pin_count(pin_bytes)?;
                if count == 0 {
                    continue;
                }
                let Some(meta_bytes) = metadata.get(&rtxn, hash).map_err(map_heed_error)? else {
                    continue;
                };
                let meta = Self::decode_blob_meta(meta_bytes)?;
                totals.pinned_count = totals.pinned_count.saturating_add(1);
                totals.pinned_bytes = totals.pinned_bytes.saturating_add(meta.size);
            }
        }

        let mut wtxn = env.write_txn().map_err(map_heed_error)?;
        Self::write_store_totals(stats, &mut wtxn, totals).map_err(map_heed_error)?;
        wtxn.commit().map_err(map_heed_error)?;
        Ok(totals)
    }

    fn read_store_totals(
        stats: Database<Bytes, Bytes>,
        txn: &heed::RoTxn,
    ) -> Result<Option<StoreTotals>, StoreError> {
        stats
            .get(txn, STORE_TOTALS_KEY)
            .map_err(map_heed_error)?
            .map(Self::decode_store_totals)
            .transpose()
    }

    fn read_store_totals_lossy(
        stats: Database<Bytes, Bytes>,
        txn: &heed::RoTxn,
    ) -> std::result::Result<StoreTotals, HeedError> {
        Ok(stats
            .get(txn, STORE_TOTALS_KEY)?
            .and_then(Self::decode_store_totals_lossy)
            .unwrap_or_default())
    }

    fn write_store_totals(
        stats: Database<Bytes, Bytes>,
        txn: &mut heed::RwTxn,
        totals: StoreTotals,
    ) -> std::result::Result<(), HeedError> {
        let encoded = Self::encode_store_totals(totals);
        stats.put(txn, STORE_TOTALS_KEY, &encoded)
    }

    fn increment_totals_in_txn(
        &self,
        txn: &mut heed::RwTxn,
        count: u64,
        total_bytes: u64,
        pinned_count: u64,
        pinned_bytes: u64,
    ) -> std::result::Result<(), HeedError> {
        let mut totals = Self::read_store_totals_lossy(self.stats, txn)?;
        totals.count = totals.count.saturating_add(count);
        totals.total_bytes = totals.total_bytes.saturating_add(total_bytes);
        totals.pinned_count = totals.pinned_count.saturating_add(pinned_count);
        totals.pinned_bytes = totals.pinned_bytes.saturating_add(pinned_bytes);
        Self::write_store_totals(self.stats, txn, totals)
    }

    fn decrement_totals_in_txn(
        &self,
        txn: &mut heed::RwTxn,
        count: u64,
        total_bytes: u64,
        pinned_count: u64,
        pinned_bytes: u64,
    ) -> std::result::Result<(), HeedError> {
        let mut totals = Self::read_store_totals_lossy(self.stats, txn)?;
        totals.count = totals.count.saturating_sub(count);
        totals.total_bytes = totals.total_bytes.saturating_sub(total_bytes);
        totals.pinned_count = totals.pinned_count.saturating_sub(pinned_count);
        totals.pinned_bytes = totals.pinned_bytes.saturating_sub(pinned_bytes);
        Self::write_store_totals(self.stats, txn, totals)
    }

    /// Check if a hash exists (sync version for internal use).
    pub fn exists(&self, hash: &Hash) -> Result<bool, StoreError> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;

        if self
            .metadata
            .get(&rtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .is_some()
        {
            return Ok(true);
        }

        Ok(self
            .blobs
            .get(&rtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .is_some())
    }

    fn mark_existing_hashes_in_db(
        db: Database<Bytes, Bytes>,
        rtxn: &heed::RoTxn,
        sorted_hashes: &[Hash],
        existing: &mut [bool],
    ) -> Result<(), StoreError> {
        debug_assert_eq!(sorted_hashes.len(), existing.len());
        debug_assert!(sorted_hashes.windows(2).all(|pair| pair[0] <= pair[1]));

        if sorted_hashes.is_empty() {
            return Ok(());
        }

        for (index, hash) in sorted_hashes.iter().enumerate() {
            if existing[index] {
                continue;
            }
            if db.get(rtxn, hash).map_err(map_heed_error)?.is_some() {
                existing[index] = true;
            }
        }

        Ok(())
    }

    /// Mark which sorted hashes already exist using bounded LMDB range scans.
    pub fn existing_hashes_in_sorted_candidates(
        &self,
        sorted_hashes: &[Hash],
    ) -> Result<Vec<bool>, StoreError> {
        let mut existing = vec![false; sorted_hashes.len()];
        if sorted_hashes.is_empty() {
            return Ok(existing);
        }

        let rtxn = self.env.read_txn().map_err(map_heed_error)?;
        Self::mark_existing_hashes_in_db(self.metadata, &rtxn, sorted_hashes, &mut existing)?;
        if existing.iter().all(|exists| *exists) {
            return Ok(existing);
        }
        Self::mark_existing_hashes_in_db(self.blobs, &rtxn, sorted_hashes, &mut existing)?;
        Ok(existing)
    }

    pub fn blob_size_sync(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        self.blob_size_in_txn(&rtxn, hash)
    }

    pub fn map_size_bytes(&self) -> usize {
        self.env.info().map_size
    }

    fn evict_before_write(&self, incoming_bytes: u64) -> Result<u64, StoreError> {
        let max = self.max_bytes.load(Ordering::Relaxed);
        if max == 0 {
            return Ok(0);
        }

        let current = self.current_bytes.load(Ordering::Relaxed);
        if current.saturating_add(incoming_bytes) <= max {
            return Ok(0);
        }

        let target = if incoming_bytes >= max {
            0
        } else {
            (max.saturating_mul(9) / 10).min(max.saturating_sub(incoming_bytes))
        };
        self.evict_to_target(current, target)
    }

    fn evict_for_write_pressure(&self, incoming_bytes: u64) -> Result<u64, StoreError> {
        let current = self.current_bytes.load(Ordering::Relaxed);
        if current == 0 {
            return Ok(0);
        }

        let headroom = incoming_bytes.max(current / 10).max(1);
        let target = current.saturating_sub(headroom);
        self.evict_to_target(current, target)
    }

    fn is_map_full_error(err: &HeedError) -> bool {
        matches!(err, HeedError::Mdb(MdbError::MapFull))
    }

    fn put_sync_attempt(&self, hash: Hash, data: &[u8]) -> std::result::Result<bool, HeedError> {
        let mut wtxn = self.env.write_txn()?;
        let inserted =
            match self
                .blobs
                .put_with_flags(&mut wtxn, PutFlags::NO_OVERWRITE, &hash, data)
            {
                Ok(()) => true,
                Err(HeedError::Mdb(MdbError::KeyExist)) => false,
                Err(err) => return Err(err),
            };

        if inserted {
            let order = self.next_order.fetch_add(1, Ordering::Relaxed);
            let meta = Self::encode_blob_meta(BlobMeta {
                order,
                size: data.len() as u64,
                last_accessed_at: unix_timestamp_now(),
            });
            let order_key = Self::encode_order_key(order, &hash);
            self.metadata.put(&mut wtxn, &hash, &meta)?;
            self.eviction_order.put(&mut wtxn, &order_key, &())?;
            let pin_count = self.read_pin_count_lossy(&wtxn, &hash)?;
            self.increment_totals_in_txn(
                &mut wtxn,
                1,
                data.len() as u64,
                u64::from(pin_count > 0),
                if pin_count > 0 { data.len() as u64 } else { 0 },
            )?;
        }

        wtxn.commit()?;
        Ok(inserted)
    }

    fn put_many_sync_attempt(
        &self,
        items: &[(Hash, Vec<u8>)],
    ) -> std::result::Result<(usize, u64), HeedError> {
        let mut wtxn = self.env.write_txn()?;
        let mut inserted = 0usize;
        let mut inserted_bytes = 0u64;
        let mut inserted_pinned = 0u64;
        let mut inserted_pinned_bytes = 0u64;

        for (hash, data) in items {
            let inserted_blob = match self.blobs.put_with_flags(
                &mut wtxn,
                PutFlags::NO_OVERWRITE,
                hash,
                data.as_slice(),
            ) {
                Ok(()) => true,
                Err(HeedError::Mdb(MdbError::KeyExist)) => false,
                Err(err) => return Err(err),
            };

            if !inserted_blob {
                continue;
            }

            let order = self.next_order.fetch_add(1, Ordering::Relaxed);
            let meta = Self::encode_blob_meta(BlobMeta {
                order,
                size: data.len() as u64,
                last_accessed_at: unix_timestamp_now(),
            });
            let order_key = Self::encode_order_key(order, hash);
            self.metadata.put(&mut wtxn, hash, &meta)?;
            self.eviction_order.put(&mut wtxn, &order_key, &())?;
            inserted += 1;
            inserted_bytes = inserted_bytes.saturating_add(data.len() as u64);
            if self.read_pin_count_lossy(&wtxn, hash)? > 0 {
                inserted_pinned = inserted_pinned.saturating_add(1);
                inserted_pinned_bytes = inserted_pinned_bytes.saturating_add(data.len() as u64);
            }
        }

        if inserted > 0 {
            self.increment_totals_in_txn(
                &mut wtxn,
                inserted as u64,
                inserted_bytes,
                inserted_pinned,
                inserted_pinned_bytes,
            )?;
        }

        wtxn.commit()?;
        Ok((inserted, inserted_bytes))
    }

    /// Get storage statistics.
    pub fn stats(&self) -> Result<LmdbStats, StoreError> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let totals = Self::read_store_totals(self.stats, &rtxn)?.unwrap_or_default();

        Ok(LmdbStats {
            count: totals.count as usize,
            total_bytes: totals.total_bytes,
            pinned_count: totals.pinned_count as usize,
            pinned_bytes: totals.pinned_bytes,
        })
    }

    /// List all hashes in the store.
    pub fn list(&self) -> Result<Vec<Hash>, StoreError> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;

        let mut hashes = Vec::new();
        for item in self.metadata.iter(&rtxn).map_err(map_heed_error)? {
            let (hash, _) = item.map_err(|e| StoreError::Other(e.to_string()))?;
            let hash_arr: Hash = hash
                .try_into()
                .map_err(|_| StoreError::Other("invalid hash length".into()))?;
            hashes.push(hash_arr);
        }

        if hashes.is_empty() {
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
        }

        Ok(hashes)
    }

    /// Sync put operation (for use in sync contexts).
    pub fn put_sync(&self, hash: Hash, data: &[u8]) -> Result<bool, StoreError> {
        let incoming_bytes = data.len() as u64;
        self.evict_before_write(incoming_bytes)?;

        let mut retried_after_eviction = false;
        loop {
            match self.put_sync_attempt(hash, data) {
                Ok(inserted) => {
                    if inserted {
                        self.current_bytes
                            .fetch_add(incoming_bytes, Ordering::Relaxed);
                    } else {
                        self.touch_accessed_sync(&hash, unix_timestamp_now())?;
                    }
                    return Ok(inserted);
                }
                Err(err) if Self::is_map_full_error(&err) && !retried_after_eviction => {
                    let freed = self.evict_for_write_pressure(incoming_bytes)?;
                    if freed == 0 {
                        return Err(StoreError::Other(err.to_string()));
                    }
                    retried_after_eviction = true;
                }
                Err(err) => return Err(StoreError::Other(err.to_string())),
            }
        }
    }

    /// Sync batch put operation (for use in sync contexts).
    pub fn put_many_sync(&self, items: &[(Hash, Vec<u8>)]) -> Result<usize, StoreError> {
        if items.is_empty() {
            return Ok(0);
        }

        let incoming_bytes = items
            .iter()
            .map(|(_, data)| data.len() as u64)
            .fold(0u64, |total, size| total.saturating_add(size));
        self.evict_before_write(incoming_bytes)?;

        let mut retried_after_eviction = false;
        loop {
            match self.put_many_sync_attempt(items) {
                Ok((inserted, inserted_bytes)) => {
                    if inserted_bytes > 0 {
                        self.current_bytes
                            .fetch_add(inserted_bytes, Ordering::Relaxed);
                    }
                    return Ok(inserted);
                }
                Err(err) if Self::is_map_full_error(&err) && !retried_after_eviction => {
                    let freed = self.evict_for_write_pressure(incoming_bytes)?;
                    if freed == 0 {
                        return Err(StoreError::Other(err.to_string()));
                    }
                    retried_after_eviction = true;
                }
                Err(err) => return Err(StoreError::Other(err.to_string())),
            }
        }
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

    pub fn get_range_sync(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let Some(blob) = self
            .blobs
            .get(&rtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
        else {
            return Ok(None);
        };

        if blob.is_empty() || end_inclusive < start {
            return Ok(Some(Vec::new()));
        }
        let len = blob.len() as u64;
        if start >= len {
            return Ok(Some(Vec::new()));
        }

        let actual_end = end_inclusive.min(len - 1);
        let start = usize::try_from(start)
            .map_err(|_| StoreError::Other("blob range start is too large".to_string()))?;
        let end_exclusive = usize::try_from(actual_end.saturating_add(1))
            .map_err(|_| StoreError::Other("blob range end is too large".to_string()))?;
        Ok(Some(blob[start..end_exclusive].to_vec()))
    }

    pub fn touch_accessed_sync(&self, hash: &Hash, now: u64) -> Result<bool, StoreError> {
        self.touch_many_accessed_sync(std::slice::from_ref(hash), now)
            .map(|updated| updated > 0)
    }

    pub fn touch_many_accessed_sync(&self, hashes: &[Hash], now: u64) -> Result<usize, StoreError> {
        if hashes.is_empty() {
            return Ok(0);
        }

        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let mut updated = 0usize;

        for hash in hashes {
            let meta = self
                .metadata
                .get(&wtxn, hash)
                .map_err(|e| StoreError::Other(e.to_string()))?
                .map(Self::decode_blob_meta)
                .transpose()?;
            let Some(mut meta) = meta else {
                continue;
            };

            if meta.last_accessed_at >= now {
                continue;
            }

            let old_order_key = Self::encode_order_key(meta.order, hash);
            let _ = self
                .eviction_order
                .delete(&mut wtxn, &old_order_key)
                .map_err(|e| StoreError::Other(e.to_string()))?;

            meta.order = self.next_order.fetch_add(1, Ordering::Relaxed);
            meta.last_accessed_at = now;
            let meta_bytes = Self::encode_blob_meta(meta);
            let new_order_key = Self::encode_order_key(meta.order, hash);
            self.metadata
                .put(&mut wtxn, hash, &meta_bytes)
                .map_err(|e| StoreError::Other(e.to_string()))?;
            self.eviction_order
                .put(&mut wtxn, &new_order_key, &())
                .map_err(|e| StoreError::Other(e.to_string()))?;
            updated += 1;
        }

        wtxn.commit()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        Ok(updated)
    }

    pub fn last_accessed_at_sync(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        self.metadata
            .get(&rtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .map(Self::decode_blob_meta)
            .transpose()
            .map(|meta| meta.map(|meta| meta.last_accessed_at))
    }

    pub fn many_last_accessed_at_sync(
        &self,
        hashes: &[Hash],
    ) -> Result<Vec<(Hash, u64)>, StoreError> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let mut results = Vec::new();
        for hash in hashes {
            let Some(meta) = self
                .metadata
                .get(&rtxn, hash)
                .map_err(|e| StoreError::Other(e.to_string()))?
                .map(Self::decode_blob_meta)
                .transpose()?
            else {
                continue;
            };
            results.push((*hash, meta.last_accessed_at));
        }
        Ok(results)
    }

    /// Sync delete operation (for use in sync contexts).
    pub fn delete_sync(&self, hash: &Hash) -> Result<bool, StoreError> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let (existed, freed) = self.delete_blob_in_txn(&mut wtxn, hash)?;

        wtxn.commit()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        if freed > 0 {
            self.current_bytes.fetch_sub(freed, Ordering::Relaxed);
        }

        Ok(existed)
    }

    fn pin_sync(&self, hash: &Hash) -> Result<(), StoreError> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let previous = self.read_pin_count(&wtxn, hash)?;
        let count = previous.saturating_add(1);
        let encoded = count.to_be_bytes();
        self.pins
            .put(&mut wtxn, hash, &encoded)
            .map_err(|e| StoreError::Other(e.to_string()))?;
        if previous == 0 {
            if let Some(size) = self.blob_size_in_txn(&wtxn, hash)? {
                self.increment_totals_in_txn(&mut wtxn, 0, 0, 1, size)
                    .map_err(|e| StoreError::Other(e.to_string()))?;
            }
        }
        wtxn.commit()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        Ok(())
    }

    fn unpin_sync(&self, hash: &Hash) -> Result<(), StoreError> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let count = self.read_pin_count(&wtxn, hash)?;
        if count == 1 {
            if let Some(size) = self.blob_size_in_txn(&wtxn, hash)? {
                self.decrement_totals_in_txn(&mut wtxn, 0, 0, 1, size)
                    .map_err(|e| StoreError::Other(e.to_string()))?;
            }
        }
        if count <= 1 {
            let _ = self
                .pins
                .delete(&mut wtxn, hash)
                .map_err(|e| StoreError::Other(e.to_string()))?;
        } else {
            let encoded = (count - 1).to_be_bytes();
            self.pins
                .put(&mut wtxn, hash, &encoded)
                .map_err(|e| StoreError::Other(e.to_string()))?;
        }
        wtxn.commit()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        Ok(())
    }

    fn evict_to_target(&self, current_bytes: u64, target_bytes: u64) -> Result<u64, StoreError> {
        if current_bytes <= target_bytes {
            return Ok(0);
        }

        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let order_keys: Vec<Vec<u8>> = self
            .eviction_order
            .iter(&rtxn)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .map(|item| {
                item.map(|(key, _)| key.to_vec())
                    .map_err(|e| StoreError::Other(e.to_string()))
            })
            .collect::<Result<_, _>>()?;
        drop(rtxn);

        let mut freed_total = 0u64;
        let to_free = current_bytes - target_bytes;
        let mut index = 0usize;

        while freed_total < to_free && index < order_keys.len() {
            let mut wtxn = self
                .env
                .write_txn()
                .map_err(|e| StoreError::Other(e.to_string()))?;
            let mut batch_freed = 0u64;
            let mut batch_items = 0usize;

            while freed_total + batch_freed < to_free && index < order_keys.len() {
                let order_key = &order_keys[index];
                index += 1;

                let hash = Self::decode_hash_from_order_key(order_key)?;
                if self.read_pin_count(&wtxn, &hash)? > 0 {
                    continue;
                }

                let (_, bytes_freed) = self.delete_blob_in_txn(&mut wtxn, &hash)?;
                if bytes_freed == 0 {
                    let _ = self
                        .eviction_order
                        .delete(&mut wtxn, order_key)
                        .map_err(|e| StoreError::Other(e.to_string()))?;
                    continue;
                }

                batch_freed = batch_freed.saturating_add(bytes_freed);
                batch_items += 1;

                if batch_freed >= EVICTION_BATCH_TARGET_BYTES
                    || batch_items >= EVICTION_BATCH_MAX_ITEMS
                {
                    break;
                }
            }

            wtxn.commit()
                .map_err(|e| StoreError::Other(e.to_string()))?;
            if batch_freed > 0 {
                self.current_bytes.fetch_sub(batch_freed, Ordering::Relaxed);
                freed_total = freed_total.saturating_add(batch_freed);
            }
        }

        Ok(freed_total)
    }

    fn delete_blob_in_txn(
        &self,
        wtxn: &mut heed::RwTxn,
        hash: &Hash,
    ) -> Result<(bool, u64), StoreError> {
        let pin_count = self.read_pin_count(wtxn, hash)?;
        let data_len = self
            .blobs
            .get(wtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .map(|data| data.len() as u64);
        let meta = self
            .metadata
            .get(wtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .map(Self::decode_blob_meta)
            .transpose()?;

        let existed = self
            .blobs
            .delete(wtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let _ = self
            .metadata
            .delete(wtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let _ = self
            .pins
            .delete(wtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?;
        if let Some(meta) = meta {
            let order_key = Self::encode_order_key(meta.order, hash);
            let _ = self
                .eviction_order
                .delete(wtxn, &order_key)
                .map_err(|e| StoreError::Other(e.to_string()))?;
        }
        let bytes_freed = data_len.or(meta.map(|m| m.size)).unwrap_or(0);
        if existed || meta.is_some() {
            self.decrement_totals_in_txn(
                wtxn,
                1,
                bytes_freed,
                u64::from(pin_count > 0),
                if pin_count > 0 { bytes_freed } else { 0 },
            )
            .map_err(|e| StoreError::Other(e.to_string()))?;
        }

        Ok((existed || meta.is_some(), bytes_freed))
    }

    fn blob_size_in_txn(&self, txn: &heed::RoTxn, hash: &Hash) -> Result<Option<u64>, StoreError> {
        if let Some(meta) = self
            .metadata
            .get(txn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .map(Self::decode_blob_meta)
            .transpose()?
        {
            return Ok(Some(meta.size));
        }

        self.blobs
            .get(txn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))
            .map(|data| data.map(|bytes| bytes.len() as u64))
    }

    fn read_pin_count(&self, txn: &heed::RoTxn, hash: &[u8]) -> Result<u32, StoreError> {
        self.pins
            .get(txn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .map(Self::decode_pin_count)
            .transpose()?
            .map_or(Ok(0), Ok)
    }

    fn read_pin_count_lossy(
        &self,
        txn: &heed::RoTxn,
        hash: &[u8],
    ) -> std::result::Result<u32, HeedError> {
        Ok(self
            .pins
            .get(txn, hash)?
            .and_then(Self::decode_pin_count_lossy)
            .unwrap_or(0))
    }

    fn encode_blob_meta(meta: BlobMeta) -> [u8; BLOB_META_BYTES] {
        let mut encoded = [0u8; BLOB_META_BYTES];
        encoded[..8].copy_from_slice(&meta.order.to_be_bytes());
        encoded[8..16].copy_from_slice(&meta.size.to_be_bytes());
        encoded[16..].copy_from_slice(&meta.last_accessed_at.to_be_bytes());
        encoded
    }

    fn decode_blob_meta(bytes: &[u8]) -> Result<BlobMeta, StoreError> {
        if bytes.len() != LEGACY_BLOB_META_BYTES && bytes.len() != BLOB_META_BYTES {
            return Err(StoreError::Other(format!(
                "invalid blob metadata length: {}",
                bytes.len()
            )));
        }
        Ok(BlobMeta {
            order: Self::decode_order(&bytes[..8])?,
            size: u64::from_be_bytes(
                bytes[8..16]
                    .try_into()
                    .map_err(|_| StoreError::Other("invalid blob size bytes".into()))?,
            ),
            last_accessed_at: if bytes.len() >= BLOB_META_BYTES {
                u64::from_be_bytes(
                    bytes[16..24]
                        .try_into()
                        .map_err(|_| StoreError::Other("invalid blob access time bytes".into()))?,
                )
            } else {
                0
            },
        })
    }

    fn encode_store_totals(totals: StoreTotals) -> [u8; STORE_TOTALS_BYTES] {
        let mut encoded = [0u8; STORE_TOTALS_BYTES];
        encoded[0..8].copy_from_slice(&totals.count.to_be_bytes());
        encoded[8..16].copy_from_slice(&totals.total_bytes.to_be_bytes());
        encoded[16..24].copy_from_slice(&totals.pinned_count.to_be_bytes());
        encoded[24..32].copy_from_slice(&totals.pinned_bytes.to_be_bytes());
        encoded
    }

    fn decode_store_totals(bytes: &[u8]) -> Result<StoreTotals, StoreError> {
        Self::decode_store_totals_lossy(bytes).ok_or_else(|| {
            StoreError::Other(format!("invalid store totals length: {}", bytes.len()))
        })
    }

    fn decode_store_totals_lossy(bytes: &[u8]) -> Option<StoreTotals> {
        if bytes.len() != STORE_TOTALS_BYTES {
            return None;
        }
        Some(StoreTotals {
            count: u64::from_be_bytes(bytes[0..8].try_into().ok()?),
            total_bytes: u64::from_be_bytes(bytes[8..16].try_into().ok()?),
            pinned_count: u64::from_be_bytes(bytes[16..24].try_into().ok()?),
            pinned_bytes: u64::from_be_bytes(bytes[24..32].try_into().ok()?),
        })
    }

    fn encode_order_key(order: u64, hash: &Hash) -> [u8; ORDER_KEY_BYTES] {
        let mut key = [0u8; ORDER_KEY_BYTES];
        key[..8].copy_from_slice(&order.to_be_bytes());
        key[8..].copy_from_slice(hash);
        key
    }

    fn decode_order(bytes: &[u8]) -> Result<u64, StoreError> {
        if bytes.len() != 8 {
            return Err(StoreError::Other(format!(
                "invalid order length: {}",
                bytes.len()
            )));
        }
        Ok(u64::from_be_bytes(bytes.try_into().map_err(|_| {
            StoreError::Other("invalid order bytes".into())
        })?))
    }

    fn decode_hash_from_order_key(bytes: &[u8]) -> Result<Hash, StoreError> {
        if bytes.len() != ORDER_KEY_BYTES {
            return Err(StoreError::Other(format!(
                "invalid order key length: {}",
                bytes.len()
            )));
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes[8..]);
        Ok(hash)
    }

    fn decode_order_from_order_key(bytes: &[u8]) -> Result<u64, StoreError> {
        if bytes.len() != ORDER_KEY_BYTES {
            return Err(StoreError::Other(format!(
                "invalid order key length: {}",
                bytes.len()
            )));
        }
        Self::decode_order(&bytes[..8])
    }

    fn decode_pin_count(bytes: &[u8]) -> Result<u32, StoreError> {
        Self::decode_pin_count_lossy(bytes)
            .ok_or_else(|| StoreError::Other(format!("invalid pin count length: {}", bytes.len())))
    }

    fn decode_pin_count_lossy(bytes: &[u8]) -> Option<u32> {
        if bytes.len() != PIN_COUNT_BYTES {
            return None;
        }
        Some(u32::from_be_bytes(bytes.try_into().ok()?))
    }
}

fn map_heed_error(error: HeedError) -> StoreError {
    match error {
        HeedError::Io(io_error) => StoreError::Io(io_error),
        other => StoreError::Other(other.to_string()),
    }
}

fn unix_timestamp_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Clone)]
pub struct LmdbStats {
    pub count: usize,
    pub total_bytes: u64,
    pub pinned_count: usize,
    pub pinned_bytes: u64,
}

#[async_trait]
impl Store for LmdbBlobStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.put_sync(hash, &data)
    }

    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.put_many_sync(&items)
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
            Ok(stats) => StoreStats {
                count: stats.count as u64,
                bytes: stats.total_bytes,
                pinned_count: stats.pinned_count as u64,
                pinned_bytes: stats.pinned_bytes,
            },
            Err(_) => StoreStats::default(),
        }
    }

    async fn evict_if_needed(&self) -> Result<u64, StoreError> {
        let max = self.max_bytes.load(Ordering::Relaxed);
        if max == 0 {
            return Ok(0);
        }

        let current = self.current_bytes.load(Ordering::Relaxed);
        if current <= max {
            return Ok(0);
        }

        let target = max * 9 / 10;
        self.evict_to_target(current, target)
    }

    async fn pin(&self, hash: &Hash) -> Result<(), StoreError> {
        self.pin_sync(hash)
    }

    async fn unpin(&self, hash: &Hash) -> Result<(), StoreError> {
        self.unpin_sync(hash)
    }

    fn pin_count(&self, hash: &Hash) -> u32 {
        let Ok(rtxn) = self.env.read_txn() else {
            return 0;
        };
        self.read_pin_count(&rtxn, hash).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_core::sha256;
    use heed::EnvOpenOptions;
    #[cfg(unix)]
    use std::path::{Path, PathBuf};
    #[cfg(unix)]
    use std::process::Command;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Barrier,
    };
    use std::time::Duration;
    use tempfile::TempDir;

    #[cfg(unix)]
    const STALE_READER_HELPER_ENV: &str = "HASHTREE_LMDB_STALE_READER_HELPER";
    #[cfg(unix)]
    const STALE_READER_HELPER_MODE_ENV: &str = "HASHTREE_LMDB_STALE_READER_HELPER_MODE";
    #[cfg(unix)]
    const STALE_READER_DB_PATH_ENV: &str = "HASHTREE_LMDB_STALE_READER_DB_PATH";
    #[cfg(unix)]
    const STALE_READER_MARKER_PATH_ENV: &str = "HASHTREE_LMDB_STALE_READER_MARKER_PATH";
    #[cfg(unix)]
    const TEST_MAX_READERS: u32 = 4;

    #[cfg(unix)]
    fn run_helper(mode: &str, path: &Path, marker: &Path) {
        let output = Command::new(std::env::current_exe().expect("test binary path"))
            .arg("--ignored")
            .arg("--exact")
            .arg("tests::lmdb_stale_reader_helper")
            .env(STALE_READER_HELPER_ENV, "1")
            .env(STALE_READER_HELPER_MODE_ENV, mode)
            .env(STALE_READER_DB_PATH_ENV, path)
            .env(STALE_READER_MARKER_PATH_ENV, marker)
            .env("RUST_TEST_THREADS", "1")
            .output()
            .expect("spawn stale-reader helper");

        assert!(
            output.status.success(),
            "stale-reader helper failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            marker.exists(),
            "stale-reader helper did not run: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

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

    #[test]
    fn existing_hashes_in_sorted_candidates_marks_present_hashes() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;

        let h1 = sha256(b"range present one");
        let h2 = sha256(b"range present two");
        let missing = sha256(b"range missing");
        store.put_sync(h1, b"range present one")?;
        store.put_sync(h2, b"range present two")?;

        let mut candidates = vec![missing, h2, h1, h1];
        candidates.sort_unstable();
        let existing = store.existing_hashes_in_sorted_candidates(&candidates)?;

        assert_eq!(existing.len(), candidates.len());
        for (hash, exists) in candidates.iter().zip(existing) {
            assert_eq!(exists, *hash == h1 || *hash == h2);
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_blob_last_accessed_persists_and_updates_eviction_order() -> Result<(), StoreError>
    {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;

        let first = sha256(b"first");
        let second = sha256(b"second");
        store.put(first, b"first".to_vec()).await?;
        store.put(second, b"second".to_vec()).await?;

        assert!(store.last_accessed_at_sync(&first)?.unwrap_or(0) > 0);
        let access_time = unix_timestamp_now().saturating_add(1000);
        store.touch_accessed_sync(&first, access_time)?;

        assert_eq!(store.last_accessed_at_sync(&first)?, Some(access_time));
        assert!(store.delete_sync(&first)?);
        assert!(store.exists(&second)?);

        Ok(())
    }

    #[test]
    fn duplicate_put_refreshes_blob_last_accessed() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;

        let data = b"already here";
        let hash = sha256(data);
        assert!(store.put_sync(hash, data)?);
        store.touch_accessed_sync(&hash, 1)?;

        assert!(!store.put_sync(hash, data)?);
        assert!(store.last_accessed_at_sync(&hash)?.unwrap_or(0) > 1);

        Ok(())
    }

    #[test]
    fn test_decodes_legacy_blob_metadata_without_access_time() -> Result<(), StoreError> {
        let mut encoded = [0u8; LEGACY_BLOB_META_BYTES];
        encoded[..8].copy_from_slice(&7u64.to_be_bytes());
        encoded[8..].copy_from_slice(&42u64.to_be_bytes());

        let meta = LmdbBlobStore::decode_blob_meta(&encoded)?;

        assert_eq!(meta.order, 7);
        assert_eq!(meta.size, 42);
        assert_eq!(meta.last_accessed_at, 0);
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
    async fn test_stats_persist_across_reopen_and_mutations() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("blobs");
        let h1 = sha256(b"hello");
        let h2 = sha256(b"world!");
        let h3 = sha256(b"prepin");

        {
            let store = LmdbBlobStore::new(&path)?;
            store.put(h1, b"hello".to_vec()).await?;
            store.put(h2, b"world!".to_vec()).await?;
            store.pin(&h1).await?;
            store.pin(&h3).await?;
            store.put(h3, b"prepin".to_vec()).await?;

            let stats = store.stats()?;
            assert_eq!(stats.count, 3);
            assert_eq!(stats.total_bytes, 17);
            assert_eq!(stats.pinned_count, 2);
            assert_eq!(stats.pinned_bytes, 11);
        }

        {
            let reopened = LmdbBlobStore::new(&path)?;
            let stats = reopened.stats()?;
            assert_eq!(stats.count, 3);
            assert_eq!(stats.total_bytes, 17);
            assert_eq!(stats.pinned_count, 2);
            assert_eq!(stats.pinned_bytes, 11);

            assert!(reopened.delete(&h1).await?);
            let stats = reopened.stats()?;
            assert_eq!(stats.count, 2);
            assert_eq!(stats.total_bytes, 12);
            assert_eq!(stats.pinned_count, 1);
            assert_eq!(stats.pinned_bytes, 6);
        }

        let reopened = LmdbBlobStore::new(&path)?;
        let stats = reopened.stats()?;
        assert_eq!(stats.count, 2);
        assert_eq!(stats.total_bytes, 12);
        assert_eq!(stats.pinned_count, 1);
        assert_eq!(stats.pinned_bytes, 6);

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

    #[tokio::test]
    async fn test_max_bytes() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;

        assert!(store.max_bytes().is_none());

        store.set_max_bytes(1000);
        assert_eq!(store.max_bytes(), Some(1000));

        store.set_max_bytes(0);
        assert!(store.max_bytes().is_none());

        Ok(())
    }

    #[test]
    fn test_with_max_bytes_expands_lmdb_map_size() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let requested = (DEFAULT_MAP_SIZE as u64) + 64 * 1024 * 1024;
        let store = LmdbBlobStore::with_max_bytes(temp.path().join("blobs"), requested)?;

        assert!(
            store.map_size_bytes() as u64 >= requested,
            "expected LMDB map to grow to at least {requested} bytes, got {}",
            store.map_size_bytes()
        );
        assert_eq!(store.max_bytes(), Some(requested));

        Ok(())
    }

    #[tokio::test]
    async fn test_eviction_over_limit() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;
        store.set_max_bytes(25);

        let h1 = sha256(b"aaaaaaaaaa");
        let h2 = sha256(b"bbbbbbbbbb");
        let h3 = sha256(b"cccccccccc");

        store.put(h1, b"aaaaaaaaaa".to_vec()).await?;
        store.put(h2, b"bbbbbbbbbb".to_vec()).await?;
        store.put(h3, b"cccccccccc".to_vec()).await?;

        let freed = store.evict_if_needed().await?;
        assert_eq!(
            freed, 0,
            "write path should have already evicted stale blobs"
        );

        assert!(
            !store.has(&h1).await?,
            "oldest blob should be evicted before the third write"
        );
        assert!(store.has(&h2).await?);
        assert!(store.has(&h3).await?);

        let stats = store.stats()?;
        assert!(
            stats.total_bytes <= 22,
            "store should be reduced to 90% target"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_eviction_respects_pins() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;
        store.set_max_bytes(25);

        let h1 = sha256(b"aaaaaaaaaa");
        let h2 = sha256(b"bbbbbbbbbb");
        let h3 = sha256(b"cccccccccc");

        store.put(h1, b"aaaaaaaaaa".to_vec()).await?;
        store.put(h2, b"bbbbbbbbbb".to_vec()).await?;
        store.pin(&h1).await?;
        store.put(h3, b"cccccccccc".to_vec()).await?;

        let freed = store.evict_if_needed().await?;
        assert_eq!(
            freed, 0,
            "write path should have already evicted stale blobs"
        );

        assert!(store.has(&h1).await?, "pinned blob must not be evicted");
        assert!(
            !store.has(&h2).await?,
            "oldest unpinned blob should be evicted before the third write"
        );
        assert!(store.has(&h3).await?);

        Ok(())
    }

    #[tokio::test]
    async fn test_reopen_with_existing_eviction_order() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("blobs");

        {
            let store = LmdbBlobStore::new(&path)?;
            let h1 = sha256(b"aaaaaaaaaa");
            let h2 = sha256(b"bbbbbbbbbb");
            store.put(h1, b"aaaaaaaaaa".to_vec()).await?;
            store.put(h2, b"bbbbbbbbbb".to_vec()).await?;
        }

        let reopened = LmdbBlobStore::new(&path)?;
        let h3 = sha256(b"cccccccccc");
        assert!(reopened.put(h3, b"cccccccccc".to_vec()).await?);
        assert!(reopened.has(&h3).await?);

        Ok(())
    }

    #[tokio::test]
    async fn test_reopen_with_lower_max_bytes_evicts_existing_blobs() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("blobs");

        {
            let store = LmdbBlobStore::new(&path)?;
            let h1 = sha256(b"aaaaaaaaaa");
            let h2 = sha256(b"bbbbbbbbbb");
            let h3 = sha256(b"cccccccccc");
            store.put(h1, b"aaaaaaaaaa".to_vec()).await?;
            store.put(h2, b"bbbbbbbbbb".to_vec()).await?;
            store.put(h3, b"cccccccccc".to_vec()).await?;
        }

        let reopened = LmdbBlobStore::with_max_bytes(&path, 25)?;
        let h1 = sha256(b"aaaaaaaaaa");
        let h2 = sha256(b"bbbbbbbbbb");
        let h3 = sha256(b"cccccccccc");

        assert!(
            !reopened.has(&h1).await?,
            "oldest blob should be evicted when reopening over the new cap"
        );
        assert!(reopened.has(&h2).await?);
        assert!(reopened.has(&h3).await?);

        let stats = reopened.stats()?;
        assert!(
            stats.total_bytes <= 22,
            "reopened store should be reduced to the 90% target"
        );

        Ok(())
    }

    #[test]
    fn test_supports_many_concurrent_readers() -> Result<(), Box<dyn std::error::Error>> {
        const READER_THREADS: usize = 160;

        let temp = TempDir::new()?;
        let store = Arc::new(LmdbBlobStore::new(temp.path().join("blobs"))?);
        let hash = sha256(b"many readers");
        store.put_sync(hash, b"many readers")?;

        let start = Arc::new(Barrier::new(READER_THREADS + 1));
        let release = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::with_capacity(READER_THREADS);

        for _ in 0..READER_THREADS {
            let env = store.env.clone();
            let start = Arc::clone(&start);
            let release = Arc::clone(&release);
            handles.push(std::thread::spawn(move || -> Result<(), String> {
                start.wait();
                let _rtxn = env.read_txn().map_err(|err| err.to_string())?;
                while !release.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(())
            }));
        }

        start.wait();
        std::thread::sleep(Duration::from_millis(50));
        release.store(true, Ordering::Relaxed);

        let results: Vec<Result<(), String>> = handles
            .into_iter()
            .map(|handle| handle.join().expect("reader thread panicked"))
            .collect();

        let failures: Vec<String> = results.into_iter().filter_map(Result::err).collect();
        assert!(
            failures.is_empty(),
            "concurrent reader failures: {}",
            failures.join(" | ")
        );
        assert!(store.exists(&hash)?);

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_reclaims_stale_reader_slots() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let path = temp.path().join("blobs");
        let data = b"hello stale readers";
        let hash = sha256(data);

        run_helper("setup", &path, &temp.path().join("setup.marker"));

        for index in 0..TEST_MAX_READERS {
            let marker = temp.path().join(format!("helper-{index}.marker"));
            run_helper("stale", &path, &marker);
        }

        let store = LmdbBlobStore::with_map_size(&path, 1024 * 1024)?;
        assert!(store.exists(&hash)?);

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_reopens_existing_env_with_larger_map_size() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let path = temp.path().join("blobs");
        run_helper("small-map", &path, &temp.path().join("small-map.marker"));

        let reopened = LmdbBlobStore::with_map_size(&path, 8 * 1024 * 1024)?;
        assert!(reopened.map_size_bytes() >= 8 * 1024 * 1024);

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_reopens_existing_env_with_smaller_requested_map_size(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let path = temp.path().join("blobs");
        let hash = sha256(b"large existing blob");
        run_helper("large-map", &path, &temp.path().join("large-map.marker"));

        let existing_size = std::fs::metadata(path.join("data.mdb"))?.len();
        assert!(
            existing_size > 1024 * 1024,
            "test setup should create an environment larger than the reopen request"
        );

        let reopened = LmdbBlobStore::with_map_size(&path, 1024 * 1024)?;
        assert!(
            reopened.map_size_bytes() as u64 >= existing_size,
            "expected map size to cover existing data.mdb size {existing_size}, got {}",
            reopened.map_size_bytes()
        );
        assert!(reopened.exists(&hash)?);

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "used as a subprocess helper by test_reclaims_stale_reader_slots"]
    fn lmdb_stale_reader_helper() {
        let Some(db_path) = std::env::var_os(STALE_READER_DB_PATH_ENV) else {
            return;
        };
        let marker_path =
            PathBuf::from(std::env::var_os(STALE_READER_MARKER_PATH_ENV).expect("marker path"));
        std::fs::write(&marker_path, b"started").expect("write helper marker");

        let _env_flag = std::env::var_os(STALE_READER_HELPER_ENV).expect("helper mode enabled");
        let mode = std::env::var(STALE_READER_HELPER_MODE_ENV).expect("helper mode");
        let db_path = PathBuf::from(db_path);
        std::fs::create_dir_all(&db_path).expect("create helper db dir");
        let helper_map_size = if mode == "large-map" {
            8 * 1024 * 1024
        } else {
            1024 * 1024
        };
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(helper_map_size)
                .max_dbs(DATABASE_COUNT)
                .max_readers(TEST_MAX_READERS)
                .open(&db_path)
                .expect("open lmdb env")
        };
        match mode.as_str() {
            "setup" => {
                let mut wtxn = env.write_txn().expect("open write txn");
                let blobs: Database<Bytes, Bytes> = env
                    .create_database(&mut wtxn, Some("blobs"))
                    .expect("create blobs database");
                let data = b"hello stale readers";
                let hash = sha256(data);
                blobs.put(&mut wtxn, &hash, data).expect("seed blob");
                wtxn.commit().expect("commit setup txn");
                std::process::exit(0);
            }
            "stale" => {
                let _rtxn = env.read_txn().expect("open read txn");
                std::process::exit(0);
            }
            "small-map" => {
                let mut wtxn = env.write_txn().expect("open write txn");
                let _blobs: Database<Bytes, Bytes> = env
                    .create_database(&mut wtxn, Some("blobs"))
                    .expect("create blobs database");
                let _metadata: Database<Bytes, Bytes> = env
                    .create_database(&mut wtxn, Some("metadata"))
                    .expect("create metadata database");
                let _eviction_order: Database<Bytes, Unit> = env
                    .create_database(&mut wtxn, Some("eviction_order"))
                    .expect("create eviction_order database");
                let _pins: Database<Bytes, Bytes> = env
                    .create_database(&mut wtxn, Some("pins"))
                    .expect("create pins database");
                wtxn.commit().expect("commit small-map setup txn");
                std::process::exit(0);
            }
            "large-map" => {
                let mut wtxn = env.write_txn().expect("open write txn");
                let blobs: Database<Bytes, Bytes> = env
                    .create_database(&mut wtxn, Some("blobs"))
                    .expect("create blobs database");
                let _metadata: Database<Bytes, Bytes> = env
                    .create_database(&mut wtxn, Some("metadata"))
                    .expect("create metadata database");
                let _eviction_order: Database<Bytes, Unit> = env
                    .create_database(&mut wtxn, Some("eviction_order"))
                    .expect("create eviction_order database");
                let _pins: Database<Bytes, Bytes> = env
                    .create_database(&mut wtxn, Some("pins"))
                    .expect("create pins database");
                let data = vec![42u8; 3 * 1024 * 1024];
                let hash = sha256(b"large existing blob");
                blobs.put(&mut wtxn, &hash, &data).expect("seed blob");
                wtxn.commit().expect("commit large-map setup txn");
                std::process::exit(0);
            }
            other => panic!("unknown helper mode: {other}"),
        }
    }
}
