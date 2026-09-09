use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::{future::join_all, poll};
use hashtree_core::{
    sha256, BlobReply, BlobRequest, BlobRoute, BlobRouteContext, MemoryStore, Store,
    StoreBlobRoute, StoreError,
};
use hashtree_network::{BlobRouteEntry, BlobRouter, BlobRouterConfig, MeshForwardingRoute};

struct InterruptedLink {
    calls: AtomicUsize,
    remote: Arc<BlobRouter>,
}

#[async_trait]
impl BlobRoute for InterruptedLink {
    async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            futures::future::pending::<()>().await;
        }
        tokio::task::yield_now().await;
        self.remote.route(request).await
    }

    async fn route_with_context(
        &self,
        request: BlobRequest,
        context: BlobRouteContext,
    ) -> Result<BlobReply, StoreError> {
        tokio::time::timeout_at(
            tokio::time::Instant::from_std(context.deadline),
            self.route(request),
        )
        .await
        .map_err(|_| StoreError::Other("link deadline expired".into()))?
    }
}

fn router(routes: Vec<BlobRouteEntry>, cache: Option<Arc<MemoryStore>>) -> Arc<BlobRouter> {
    Arc::new(
        BlobRouter::new(
            routes,
            cache.map(|s| s as Arc<dyn Store>),
            BlobRouterConfig::default(),
        )
        .unwrap(),
    )
}

#[tokio::test]
async fn cancelling_one_reader_does_not_fail_other_coalesced_mesh_reads() {
    let data = vec![42; 4096];
    let hash = sha256(&data);
    let source = Arc::new(MemoryStore::new());
    source.put(hash, data.clone()).await.unwrap();
    let link = Arc::new(InterruptedLink {
        calls: AtomicUsize::new(0),
        remote: router(
            vec![BlobRouteEntry::new(
                "local",
                Arc::new(StoreBlobRoute::new(source)),
            )],
            None,
        ),
    });
    let mesh = Arc::new(MeshForwardingRoute::new(link.clone()));
    let reader = router(vec![BlobRouteEntry::new("mesh", mesh)], None);
    let request = BlobRequest { hash, htl: 1 };
    let context = BlobRouteContext {
        deadline: Instant::now() + Duration::from_secs(2),
        attempt_budget: 4,
    };
    let mut owner = Box::pin(reader.get_request(request, None, Some(context)));
    assert!(poll!(owner.as_mut()).is_pending());
    let mut others = Box::pin(join_all(
        (0..16).map(|_| reader.get_request(request, None, Some(context))),
    ));
    assert!(poll!(others.as_mut()).is_pending());
    assert_eq!(link.calls.load(Ordering::SeqCst), 1);
    drop(owner);
    let replies = tokio::time::timeout(Duration::from_secs(1), others)
        .await
        .unwrap();
    let delivered = replies
        .iter()
        .filter(|reply| matches!(reply, Ok(Some(bytes)) if *bytes == data))
        .count();
    println!(
        "cancelled_owner: delivered={delivered}/16 link_attempts={}",
        link.calls.load(Ordering::SeqCst)
    );
    assert_eq!(
        delivered, 16,
        "one reader's cancellation poisoned the remaining readers: {replies:?}"
    );
    assert_eq!(link.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn earlier_owner_deadline_does_not_shorten_another_readers_search() {
    let data = b"survives the first reader's deadline".to_vec();
    let hash = sha256(&data);
    let source = Arc::new(MemoryStore::new());
    source.put(hash, data.clone()).await.unwrap();
    let link = Arc::new(InterruptedLink {
        calls: AtomicUsize::new(0),
        remote: router(
            vec![BlobRouteEntry::new(
                "local",
                Arc::new(StoreBlobRoute::new(source)),
            )],
            None,
        ),
    });
    let mesh = MeshForwardingRoute::new(link.clone());
    let request = BlobRequest { hash, htl: 1 };
    let context = BlobRouteContext {
        deadline: Instant::now() + Duration::from_millis(10),
        attempt_budget: 4,
    };
    let mut owner = Box::pin(mesh.route_with_context(request, context));
    assert!(poll!(owner.as_mut()).is_pending());
    let waiter = mesh.route_with_context(
        request,
        BlobRouteContext {
            deadline: Instant::now() + Duration::from_secs(1),
            ..context
        },
    );
    let (first, second) = tokio::join!(owner, waiter);
    assert!(first.is_err());
    assert_eq!(second.unwrap(), BlobReply::Data(data));
    assert_eq!(link.calls.load(Ordering::SeqCst), 2);
}
