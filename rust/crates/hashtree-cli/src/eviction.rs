use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::storage::HashtreeStore;

pub const BACKGROUND_EVICTION_INTERVAL: Duration = Duration::from_secs(300);

pub fn spawn_background_eviction_task(
    store: Arc<HashtreeStore>,
    interval: Duration,
    context: &'static str,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            run_background_eviction_pass(store.as_ref(), context);
        }
    })
}

fn run_background_eviction_pass(store: &HashtreeStore, context: &str) {
    match store.evict_if_needed() {
        Ok(freed) => {
            if freed > 0 {
                tracing::info!("{} background eviction freed {} bytes", context, freed);
            }
        }
        Err(err) => {
            tracing::warn!("{} background eviction error: {}", context, err);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use hashtree_config::StorageBackend;
    use hashtree_core::from_hex;
    use tempfile::TempDir;

    use super::spawn_background_eviction_task;
    use crate::storage::{HashtreeStore, PRIORITY_OTHER};

    #[tokio::test]
    async fn background_eviction_task_evicts_over_limit_tree() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            HashtreeStore::with_options_and_backend(
                temp_dir.path(),
                None,
                512,
                true,
                &StorageBackend::Fs,
            )
            .expect("create store"),
        );

        let hash_hex = store.put_blob(&vec![7u8; 1024]).expect("put blob");
        let hash = from_hex(&hash_hex).expect("decode hash");

        store
            .index_tree(&hash, "owner", Some("tree"), PRIORITY_OTHER, None)
            .expect("index tree");
        assert!(store.get_tree_meta(&hash).expect("read meta").is_some());

        let handle =
            spawn_background_eviction_task(Arc::clone(&store), Duration::from_millis(10), "test");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if store.get_tree_meta(&hash).expect("read meta").is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("background eviction timed out");

        handle.abort();
    }
}
