//! Normal `Store` facade for application-owned writes and routed reads.

use std::sync::Arc;

use async_trait::async_trait;
use hashtree_core::store::StoreStats;
use hashtree_core::{Hash, Store, StoreError};

use crate::BlobRouter;

/// A store whose reads use one hash-verifying [`BlobRouter`] while every
/// mutation remains explicitly owned by the application's primary store.
///
/// The primary store should normally also be a route and cache configured on
/// the router. This adapter never discovers a write destination and does not
/// route writes.
pub struct RoutedStore<S: Store + ?Sized> {
    primary: Arc<S>,
    router: Arc<BlobRouter>,
}

impl<S: Store + ?Sized> RoutedStore<S> {
    pub fn new(primary: Arc<S>, router: Arc<BlobRouter>) -> Self {
        Self { primary, router }
    }

    pub fn primary(&self) -> &Arc<S> {
        &self.primary
    }

    pub fn router(&self) -> &Arc<BlobRouter> {
        &self.router
    }
}

#[async_trait]
impl<S: Store + ?Sized + 'static> Store for RoutedStore<S> {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.primary.put(hash, data).await
    }

    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.primary.put_many(items).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.router.get(hash, None).await
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        Ok(self.get(hash).await?.is_some())
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.primary.delete(hash).await
    }

    fn set_max_bytes(&self, max: u64) {
        self.primary.set_max_bytes(max);
    }

    fn max_bytes(&self) -> Option<u64> {
        self.primary.max_bytes()
    }

    async fn stats(&self) -> StoreStats {
        self.primary.stats().await
    }

    async fn evict_if_needed(&self) -> Result<u64, StoreError> {
        self.primary.evict_if_needed().await
    }

    async fn pin(&self, hash: &Hash) -> Result<(), StoreError> {
        self.primary.pin(hash).await
    }

    async fn unpin(&self, hash: &Hash) -> Result<(), StoreError> {
        self.primary.unpin(hash).await
    }

    fn pin_count(&self, hash: &Hash) -> u32 {
        self.primary.pin_count(hash)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::{BlobRouteEntry, BlobRouterConfig};
    use hashtree_core::{sha256, BlobReply, BlobRequest, BlobRoute, MemoryStore};

    struct DataRoute {
        data: Vec<u8>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl BlobRoute for DataRoute {
        async fn route(&self, _request: BlobRequest) -> Result<BlobReply, StoreError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(BlobReply::Data(self.data.clone()))
        }
    }

    fn router(route: Arc<dyn BlobRoute>, cache: Arc<MemoryStore>) -> Arc<BlobRouter> {
        Arc::new(
            BlobRouter::new(
                vec![BlobRouteEntry::new("remote", route)],
                Some(cache),
                BlobRouterConfig {
                    request_timeout: Duration::from_secs(1),
                    ..Default::default()
                },
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn reads_use_the_router_and_its_verified_primary_cache() {
        let data = b"routed store data".to_vec();
        let hash = sha256(&data);
        let primary = Arc::new(MemoryStore::new());
        let remote = Arc::new(DataRoute {
            data: data.clone(),
            calls: AtomicUsize::new(0),
        });
        let store = RoutedStore::new(primary.clone(), router(remote.clone(), primary.clone()));

        assert_eq!(store.get(&hash).await.unwrap(), Some(data.clone()));
        assert_eq!(primary.get(&hash).await.unwrap(), Some(data));
        assert_eq!(remote.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn mutations_only_touch_the_explicit_primary_store() {
        let remote_data = b"remote".to_vec();
        let remote = Arc::new(DataRoute {
            data: remote_data.clone(),
            calls: AtomicUsize::new(0),
        });
        let primary = Arc::new(MemoryStore::new());
        let store = RoutedStore::new(primary.clone(), router(remote.clone(), primary.clone()));
        let local_data = b"local write".to_vec();
        let hash = sha256(&local_data);

        assert!(store.put(hash, local_data.clone()).await.unwrap());
        assert_eq!(primary.get(&hash).await.unwrap(), Some(local_data));
        store.pin(&hash).await.unwrap();
        assert_eq!(store.pin_count(&hash), 1);
        assert!(store.delete(&hash).await.unwrap());
        assert_eq!(remote.calls.load(Ordering::SeqCst), 0);
    }
}
