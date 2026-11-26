use futures::stream::{self, StreamExt};
use hashtree_blossom::BlossomStore;
use hashtree_core::{Hash, Store, StoreError};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::debug;

const PREFETCH_BATCH_HASHES: usize = 64;
const PREFETCH_MAX_CONCURRENT_BATCHES: usize = 4;
const PREFETCH_WRITE_BATCH_ITEMS: usize = 128;
const PREFETCH_WRITE_BATCH_BYTES: usize = 8 * 1024 * 1024;

pub(super) struct CachedStore {
    local: Arc<dyn Store + Send + Sync>,
    blossom: BlossomStore,
    batch_supported: AtomicBool,
}

impl CachedStore {
    pub(super) fn new(local: Arc<dyn Store + Send + Sync>, blossom: BlossomStore) -> Self {
        Self {
            local,
            blossom,
            batch_supported: AtomicBool::new(true),
        }
    }

    /// Prefetch any locally-missing hashes from Blossom using `POST /blob/batch`
    /// (falling back to parallel hedged `GET`s on plain Blossom servers) and
    /// commit them into the local store in batched `put_many` transactions.
    pub(super) async fn prefetch_missing(&self, hashes: &[Hash], concurrency: usize) {
        if hashes.is_empty() || self.blossom.client().read_servers().is_empty() {
            return;
        }

        let mut unique = Vec::with_capacity(hashes.len());
        let mut seen = HashSet::with_capacity(hashes.len());
        for &hash in hashes {
            if seen.insert(hash) {
                unique.push(hash);
            }
        }

        // Filter out hashes already present in local store.
        let mut missing = Vec::new();
        for hash in unique {
            if !matches!(self.local.has(&hash).await, Ok(true)) {
                missing.push(hash);
            }
        }
        if missing.is_empty() {
            return;
        }

        let concurrency = concurrency.max(1);
        let client = self.blossom.client();
        let mut write_buf: Vec<(Hash, Vec<u8>)> = Vec::new();
        let mut write_bytes = 0usize;
        let mut remaining_for_single: Vec<Hash> = Vec::new();

        // Try `/blob/batch` when fetching multiple blobs. Remember 404/unsupported
        // across calls on this CachedStore so plain Blossom servers are only probed once.
        let mut batch_supported =
            missing.len() >= 2 && self.batch_supported.load(Ordering::Relaxed);
        let mut chunks = missing.chunks(PREFETCH_BATCH_HASHES);

        if batch_supported {
            if let Some(first_chunk) = chunks.next() {
                let hexes: Vec<String> = first_chunk.iter().map(hex::encode).collect();
                match client.download_batch(&hexes).await {
                    Ok(Some(items)) => {
                        let mut got = HashSet::with_capacity(items.len());
                        for (h, data) in items {
                            got.insert(h);
                            write_bytes += data.len();
                            write_buf.push((h, data));
                        }
                        for &h in first_chunk {
                            if !got.contains(&h) {
                                remaining_for_single.push(h);
                            }
                        }
                        Self::flush_write_buf(&self.local, &mut write_buf, &mut write_bytes, false)
                            .await;
                    }
                    Ok(None) => {
                        batch_supported = false;
                        self.batch_supported.store(false, Ordering::Relaxed);
                        remaining_for_single.extend_from_slice(first_chunk);
                    }
                    Err(err) => {
                        debug!(
                            "Blossom batch prefetch failed, falling back to GET: {}",
                            err
                        );
                        batch_supported = false;
                        remaining_for_single.extend_from_slice(first_chunk);
                    }
                }
            }
        }

        if batch_supported {
            let rest: Vec<Vec<Hash>> = chunks.map(|c| c.to_vec()).collect();
            if !rest.is_empty() {
                let mut batch_stream = stream::iter(rest.into_iter().map(|chunk| async move {
                    let hexes: Vec<String> = chunk.iter().map(hex::encode).collect();
                    let res = client.download_batch(&hexes).await;
                    (chunk, res)
                }))
                .buffer_unordered(PREFETCH_MAX_CONCURRENT_BATCHES);

                while let Some((chunk, res)) = batch_stream.next().await {
                    match res {
                        Ok(Some(items)) => {
                            let mut got = HashSet::with_capacity(items.len());
                            for (h, data) in items {
                                got.insert(h);
                                write_bytes += data.len();
                                write_buf.push((h, data));
                            }
                            for h in chunk {
                                if !got.contains(&h) {
                                    remaining_for_single.push(h);
                                }
                            }
                            Self::flush_write_buf(
                                &self.local,
                                &mut write_buf,
                                &mut write_bytes,
                                false,
                            )
                            .await;
                        }
                        Ok(None) | Err(_) => {
                            remaining_for_single.extend(chunk);
                        }
                    }
                }
            }
        } else {
            for chunk in chunks {
                remaining_for_single.extend_from_slice(chunk);
            }
        }

        // Plain Blossom fallback (or secondary-server recovery for partial batch hits).
        if !remaining_for_single.is_empty() {
            let mut single_stream =
                stream::iter(remaining_for_single.into_iter().map(|hash| async move {
                    let key = hex::encode(hash);
                    match client.try_download_result(&key).await {
                        Ok(Some(data)) => Some((hash, data)),
                        _ => None,
                    }
                }))
                .buffer_unordered(concurrency);

            while let Some(item) = single_stream.next().await {
                if let Some((h, data)) = item {
                    write_bytes += data.len();
                    write_buf.push((h, data));
                    Self::flush_write_buf(&self.local, &mut write_buf, &mut write_bytes, false)
                        .await;
                }
            }
        }

        Self::flush_write_buf(&self.local, &mut write_buf, &mut write_bytes, true).await;
    }

    async fn flush_write_buf(
        local: &Arc<dyn Store + Send + Sync>,
        buf: &mut Vec<(Hash, Vec<u8>)>,
        bytes: &mut usize,
        force: bool,
    ) {
        if buf.is_empty() {
            return;
        }
        if !force && buf.len() < PREFETCH_WRITE_BATCH_ITEMS && *bytes < PREFETCH_WRITE_BATCH_BYTES {
            return;
        }
        let items = std::mem::take(buf);
        *bytes = 0;
        let _ = local.put_many(items).await;
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
