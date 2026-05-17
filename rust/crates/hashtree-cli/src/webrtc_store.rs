use anyhow::{anyhow, Result};
use hashtree_core::from_hex;
use std::sync::Arc;

use crate::blob_cache::BlobCache;
use crate::storage::HashtreeStore;

/// Daemon-side content adapter for the mesh router.
///
/// The storage engine stays a plain key/value reader. This wrapper owns
/// request-level policy that is useful for network serving: a bounded cache for
/// immutable chunk bodies and a short-lived negative cache for repeated misses.
pub struct CachedWebRtcContentStore {
    store: Arc<HashtreeStore>,
    cache: BlobCache,
}

impl CachedWebRtcContentStore {
    pub fn new(store: Arc<HashtreeStore>) -> Self {
        Self {
            store,
            cache: BlobCache::from_env(),
        }
    }
}

impl crate::webrtc::ContentStore for CachedWebRtcContentStore {
    fn get(&self, hash_hex: &str) -> Result<Option<Vec<u8>>> {
        if let Some(data) = self.cache.get_body(hash_hex) {
            return Ok(Some(data));
        }
        if let Some(None) = self.cache.get_size(hash_hex) {
            return Ok(None);
        }

        let hash = from_hex(hash_hex).map_err(|e| anyhow!("Invalid hash: {}", e))?;
        let data = self.store.get_chunk(&hash)?;
        self.cache.put_size(
            hash_hex.to_string(),
            data.as_ref().map(|bytes| bytes.len() as u64),
        );
        if let Some(ref bytes) = data {
            self.cache.put_body(hash_hex.to_string(), bytes);
        }
        Ok(data)
    }
}
