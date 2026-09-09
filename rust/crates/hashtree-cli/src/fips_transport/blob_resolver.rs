use std::sync::Arc;

use hashtree_core::{BlobReply, BlobRequest, BlobRoute, BlobRouteContext, StoreError};
use hashtree_network::BlobRouter;

// StorageRouter can synchronously fall back to S3. Use the existing bounded
// storage queue so a preferred store cannot block mesh hedges or their timers.
pub(super) struct BlockingStoreRoute(pub(super) Arc<dyn BlobRoute>);

#[async_trait::async_trait]
impl BlobRoute for BlockingStoreRoute {
    async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError> {
        let inner = self.0.clone();
        crate::server::run_blob_read(move || futures::executor::block_on(inner.route(request)))
            .await
            .map_err(|error| StoreError::Other(error.to_string()))?
    }
}

pub(super) struct DaemonInboundBlobRoute(pub(super) Arc<BlobRouter>);

#[async_trait::async_trait]
impl BlobRoute for DaemonInboundBlobRoute {
    async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError> {
        read_daemon_blob(&self.0, request, None).await
    }

    async fn route_with_context(
        &self,
        request: BlobRequest,
        context: BlobRouteContext,
    ) -> Result<BlobReply, StoreError> {
        read_daemon_blob(&self.0, request, Some(context)).await
    }
}

pub(crate) async fn read_daemon_blob(
    resolver: &BlobRouter,
    request: BlobRequest,
    context: Option<BlobRouteContext>,
) -> Result<BlobReply, StoreError> {
    Ok(
        match resolver
            .get_request(request, Some(&["configured-store".into()]), context)
            .await?
        {
            Some(data) => BlobReply::Data(data),
            None => BlobReply::NoResult,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_core::{sha256, MemoryStore, Store, StoreBlobRoute};
    use hashtree_network::{BlobRouteEntry, BlobRouterConfig};
    use std::sync::{mpsc, Mutex};
    use std::time::{Duration, Instant};
    use tokio::sync::Notify;

    struct BlockedStore {
        release: Mutex<mpsc::Receiver<()>>,
        finished: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl BlobRoute for BlockedStore {
        async fn route(&self, _request: BlobRequest) -> Result<BlobReply, StoreError> {
            let _ = self
                .release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(2));
            self.finished.notify_one();
            Err(StoreError::Other("store read failed".into()))
        }
    }

    #[tokio::test]
    async fn preferred_blocking_store_does_not_delay_healthy_mesh_hedge() {
        let data = b"healthy mesh while storage is blocked".to_vec();
        let hash = sha256(&data);
        let source = Arc::new(MemoryStore::new());
        source.put(hash, data.clone()).await.unwrap();
        let (release, receiver) = mpsc::channel();
        let finished = Arc::new(Notify::new());
        let router = BlobRouter::new(
            vec![
                BlobRouteEntry::new(
                    "configured-store",
                    Arc::new(BlockingStoreRoute(Arc::new(BlockedStore {
                        release: Mutex::new(receiver),
                        finished: finished.clone(),
                    }))),
                ),
                BlobRouteEntry::new("mesh", Arc::new(StoreBlobRoute::new(source))),
            ],
            None,
            BlobRouterConfig {
                hedge_delay: Duration::from_millis(5),
                request_timeout: Duration::from_secs(1),
                ..Default::default()
            },
        )
        .unwrap();
        let started = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_millis(500),
            read_daemon_blob(&router, BlobRequest { hash, htl: 3 }, None),
        )
        .await;
        // Release the blocking closure even when an assertion below fails.
        let _ = release.send(());
        tokio::time::timeout(Duration::from_secs(1), finished.notified())
            .await
            .unwrap();
        assert_eq!(result.unwrap().unwrap(), BlobReply::Data(data));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn inbound_preference_preserves_deadline_and_failure_semantics() {
        let (release, receiver) = mpsc::channel();
        let finished = Arc::new(Notify::new());
        let router = Arc::new(
            BlobRouter::new(
                vec![BlobRouteEntry::new(
                    "configured-store",
                    Arc::new(BlockingStoreRoute(Arc::new(BlockedStore {
                        release: Mutex::new(receiver),
                        finished: finished.clone(),
                    }))),
                )],
                None,
                BlobRouterConfig::default(),
            )
            .unwrap(),
        );
        let route = DaemonInboundBlobRoute(router);
        let result = route
            .route_with_context(
                BlobRequest {
                    hash: sha256(b"unavailable"),
                    htl: 3,
                },
                BlobRouteContext {
                    deadline: Instant::now() + Duration::from_millis(20),
                    attempt_budget: 1,
                },
            )
            .await;
        let _ = release.send(());
        tokio::time::timeout(Duration::from_secs(1), finished.notified())
            .await
            .unwrap();
        assert!(result.unwrap_err().to_string().contains("deadline"));
    }
}
