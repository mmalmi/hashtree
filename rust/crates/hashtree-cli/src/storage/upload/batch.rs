use super::{StorageRouter, DIR_UPLOAD_WRITE_BATCH_BYTES};
use async_trait::async_trait;
use hashtree_core::{store::StoreError, types::Hash, Store};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// Bound metadata too when an import contains many tiny files or directory nodes.
const MAX_BATCH_ITEMS: usize = 1024;

#[derive(Default)]
struct Pending {
    items: Vec<(Hash, Vec<u8>)>,
    positions: HashMap<Hash, usize>,
    bytes: usize,
}

/// Directory imports supply the source bytes even on a repeat. Use the ordinary
/// backend batch so that a catalogued but damaged PoolStore copy is repaired.
pub(super) struct DirectoryWriteBatch {
    base: Arc<StorageRouter>,
    pending: Mutex<Pending>,
}

impl DirectoryWriteBatch {
    pub(super) fn new(base: Arc<StorageRouter>) -> Self {
        Self {
            base,
            pending: Mutex::new(Pending::default()),
        }
    }

    fn commit(&self, pending: &mut Pending) -> Result<usize, StoreError> {
        if pending.items.is_empty() {
            return Ok(0);
        }
        // Keep pending visible until the synchronous router write has succeeded.
        // Concurrent workers share one batch and apply backpressure at this point.
        let inserted = self.base.put_many_sync(&pending.items)?;
        pending.items.clear();
        pending.positions.clear();
        pending.bytes = 0;
        Ok(inserted)
    }

    pub(super) fn flush(&self) -> Result<usize, StoreError> {
        self.commit(&mut self.pending.lock().unwrap())
    }

    fn enqueue(
        &self,
        items: impl IntoIterator<Item = (Hash, Vec<u8>)>,
    ) -> Result<usize, StoreError> {
        let mut pending = self.pending.lock().unwrap();
        let mut queued = 0;
        for (hash, data) in items {
            if pending.positions.contains_key(&hash) {
                continue;
            }
            if pending.bytes.saturating_add(data.len()) > DIR_UPLOAD_WRITE_BATCH_BYTES {
                self.commit(&mut pending)?;
            }
            let position = pending.items.len();
            pending.positions.insert(hash, position);
            pending.bytes += data.len();
            pending.items.push((hash, data));
            queued += 1;
            // A single oversized blob is written on its own. Smaller blobs share
            // the same byte and item limits across all files and directory nodes.
            if pending.bytes >= DIR_UPLOAD_WRITE_BATCH_BYTES
                || pending.items.len() >= MAX_BATCH_ITEMS
            {
                self.commit(&mut pending)?;
            }
        }
        Ok(queued)
    }
}

#[async_trait]
impl Store for DirectoryWriteBatch {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        Ok(self.enqueue(std::iter::once((hash, data)))? != 0)
    }

    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.enqueue(items)
    }

    async fn flush_pending(&self) -> Result<usize, StoreError> {
        self.flush()
    }

    async fn flush_pending_if_needed(&self) -> Result<usize, StoreError> {
        // Enqueue enforces both limits, including within a single streamed file.
        Ok(0)
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        {
            let pending = self.pending.lock().unwrap();
            if let Some(&i) = pending.positions.get(hash) {
                return Ok(Some(pending.items[i].1.clone()));
            }
        }
        self.base.get_sync(hash)
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        if self.pending.lock().unwrap().positions.contains_key(hash) {
            return Ok(true);
        }
        self.base.exists(hash)
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        let mut pending = self.pending.lock().unwrap();
        self.commit(&mut pending)?;
        self.base.delete_sync(hash)
    }
}
