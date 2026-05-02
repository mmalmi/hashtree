use crate::git::storage::LocalStore;
use anyhow::Result;
use hashtree_config::Config;
use hashtree_core::{MemoryStore, Store};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::warn;

pub(super) fn get_hashtree_data_dir() -> PathBuf {
    hashtree_config::get_data_dir()
}

pub(super) fn queue_hash_if_new(
    queue: &mut Vec<([u8; 32], Option<[u8; 32]>)>,
    queued: &mut HashSet<[u8; 32]>,
    hash: [u8; 32],
    key: Option<[u8; 32]>,
) -> bool {
    if queued.insert(hash) {
        queue.push((hash, key));
        true
    } else {
        false
    }
}

pub(super) fn create_local_store(path: &Path) -> Result<Arc<dyn Store + Send + Sync>> {
    let config = Config::load_or_default();
    let max_size_bytes = config
        .storage
        .max_size_gb
        .saturating_mul(1024 * 1024 * 1024);
    let store = LocalStore::new_for_backend(path, config.storage.backend, max_size_bytes)?;
    Ok(Arc::new(store))
}

pub(super) fn create_cached_local_store(path: &Path) -> (Arc<dyn Store + Send + Sync>, bool) {
    match create_local_store(path) {
        Ok(store) => (store, true),
        Err(err) => {
            warn!(
                "Shared git blob cache unavailable at {}; using an in-memory cache for this operation: {:#}",
                path.display(),
                err
            );
            (Arc::new(MemoryStore::new()), false)
        }
    }
}

pub(super) fn build_repo_viewer_url(path: &str, url_secret: Option<&[u8; 32]>) -> String {
    match url_secret {
        Some(secret) => format!("https://git.iris.to/#/{}?k={}", path, hex::encode(secret)),
        None => format!("https://git.iris.to/#/{}", path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_core::sha256;
    use tempfile::TempDir;

    #[tokio::test]
    async fn cached_local_store_falls_back_to_memory_when_shared_cache_cannot_open() {
        let temp = TempDir::new().expect("temp dir");
        let blocked_cache_path = temp.path().join("blobs");
        std::fs::write(&blocked_cache_path, b"not a directory").expect("block cache path");

        let (store, is_shared_cache) = create_cached_local_store(&blocked_cache_path);

        assert!(!is_shared_cache);
        let hash = sha256(b"fallback cache still works");
        assert!(store
            .put(hash, b"fallback cache still works".to_vec())
            .await
            .expect("put in fallback cache"));
        assert_eq!(
            store
                .get(&hash)
                .await
                .expect("read fallback cache")
                .as_deref(),
            Some(b"fallback cache still works".as_slice())
        );
    }
}
