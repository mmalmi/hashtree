use std::sync::Arc;

use hashtree_core::{BlobReply, BlobRequest, BlobRoute, BlobRouteContext, StoreError};
use hashtree_network::{BlobRouteCost, BlobRouter};

// StorageRouter can synchronously fall back to S3. Use the existing bounded
// storage queue so a preferred store cannot block mesh hedges or their timers.
pub(crate) struct BlockingStoreRoute(pub(crate) Arc<dyn BlobRoute>);

#[async_trait::async_trait]
impl BlobRoute for BlockingStoreRoute {
    async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError> {
        let inner = self.0.clone();
        crate::server::run_blob_read(move || futures::executor::block_on(inner.route(request)))
            .await
            .map_err(|error| StoreError::Other(error.to_string()))?
    }
}

/// Serves a peer's request from the daemon's router. A peer's request never
/// reaches a paid route: the daemon does not buy blobs for others.
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

/// The free blob service of a paying-only provider: nothing for anyone.
pub(super) struct NoFreeBlobs;

#[async_trait::async_trait]
impl BlobRoute for NoFreeBlobs {
    async fn route(&self, _request: BlobRequest) -> Result<BlobReply, StoreError> {
        Ok(BlobReply::NoResult)
    }
}

/// Read through the free routes of the daemon's router. It never spends
/// money, whoever asks.
pub(crate) async fn read_daemon_blob(
    resolver: &BlobRouter,
    request: BlobRequest,
    context: Option<BlobRouteContext>,
) -> Result<BlobReply, StoreError> {
    Ok(
        match resolver
            .get_request_at_cost(
                request,
                Some(&["configured-store".into()]),
                context,
                BlobRouteCost::Free,
            )
            .await?
        {
            Some(data) => BlobReply::Data(data),
            None => BlobReply::NoResult,
        },
    )
}

/// Ask the paid routes of the daemon's router, one after another. Call it
/// only after every free source of the read has finished without data.
pub(crate) async fn buy_daemon_blob(
    resolver: &BlobRouter,
    request: BlobRequest,
) -> Result<BlobReply, StoreError> {
    Ok(
        match resolver
            .get_request_at_cost(request, None, None, BlobRouteCost::Paid)
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
    async fn a_peers_request_never_reaches_a_paid_route() {
        let data = b"only the paid provider has it".to_vec();
        let hash = sha256(&data);
        let paid_store = Arc::new(MemoryStore::new());
        paid_store.put(hash, data.clone()).await.unwrap();
        let router = Arc::new(
            BlobRouter::new(
                vec![
                    BlobRouteEntry::new(
                        "configured-store",
                        Arc::new(StoreBlobRoute::new(Arc::new(MemoryStore::new()))),
                    ),
                    BlobRouteEntry::paid("paid:archive", Arc::new(StoreBlobRoute::new(paid_store))),
                ],
                None,
                BlobRouterConfig::default(),
            )
            .unwrap(),
        );
        let request = BlobRequest { hash, htl: 3 };

        let for_a_peer = DaemonInboundBlobRoute(router.clone()).route(request).await;
        assert_eq!(for_a_peer.unwrap(), BlobReply::NoResult);
        assert_eq!(
            NoFreeBlobs.route(request).await.unwrap(),
            BlobReply::NoResult
        );

        let free = read_daemon_blob(&router, request, None).await;
        assert_eq!(free.unwrap(), BlobReply::NoResult);
        let bought = buy_daemon_blob(&router, request).await;
        assert_eq!(bought.unwrap(), BlobReply::Data(data));
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
