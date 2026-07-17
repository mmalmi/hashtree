//! Hashtree mesh-forwarding ownership for the blob hop budget.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hashtree_core::{BlobReply, BlobRequest, BlobRoute, BlobRouteContext, Hash, StoreError};
use tokio::sync::watch;

const MAX_TRACKED_MESH_FORWARDS: usize = 1_024;

type SharedRouteResult = Result<BlobReply, String>;

struct ActiveForward {
    htl: u8,
    attempt_budget: Option<usize>,
    result: watch::Sender<Option<SharedRouteResult>>,
}

enum ForwardOwnership {
    Owner(ForwardOwnerGuard),
    Wait(Arc<ActiveForward>),
    SuppressCycle,
    Untracked,
}

struct ForwardOwnerGuard {
    active: Arc<Mutex<HashMap<Hash, Arc<ActiveForward>>>>,
    hash: Hash,
    forward: Arc<ActiveForward>,
    completed: bool,
}

impl ForwardOwnerGuard {
    fn complete(mut self, result: &Result<BlobReply, StoreError>) {
        let shared = match result {
            Ok(reply) => Ok(reply.clone()),
            Err(error) => Err(error.to_string()),
        };
        self.forward.result.send_replace(Some(shared));
        self.remove();
        self.completed = true;
    }

    fn remove(&self) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if active
            .get(&self.hash)
            .is_some_and(|current| Arc::ptr_eq(current, &self.forward))
        {
            active.remove(&self.hash);
        }
    }
}

impl Drop for ForwardOwnerGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        self.forward.result.send_replace(Some(Err(
            "mesh forwarding owner was cancelled before completion".to_string(),
        )));
        self.remove();
    }
}

/// Marks exactly one Hashtree mesh-forwarding decision.
///
/// Transport routes remain opaque and preserve the request they carry. Local
/// and terminal routes must not use this adapter. An exhausted request is a
/// route-local miss, so the outer search may still try other authorities.
pub struct MeshForwardingRoute {
    inner: Arc<dyn BlobRoute>,
    active: Arc<Mutex<HashMap<Hash, Arc<ActiveForward>>>>,
}

impl MeshForwardingRoute {
    pub fn new(inner: Arc<dyn BlobRoute>) -> Self {
        Self {
            inner,
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn forwarded_request(request: BlobRequest) -> Option<BlobRequest> {
        Some(BlobRequest {
            hash: request.hash,
            htl: request.htl.checked_sub(1)?,
        })
    }

    fn claim_forward(
        &self,
        request: BlobRequest,
        context: Option<BlobRouteContext>,
    ) -> ForwardOwnership {
        let attempt_budget = context.map(|context| context.attempt_budget);
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(current) = active.get(&request.hash) {
            if request.htl < current.htl {
                return ForwardOwnership::SuppressCycle;
            }
            if request.htl == current.htl && attempt_budget == current.attempt_budget {
                return ForwardOwnership::Wait(current.clone());
            }
        }
        if active.len() >= MAX_TRACKED_MESH_FORWARDS && !active.contains_key(&request.hash) {
            return ForwardOwnership::Untracked;
        }
        let (result, _) = watch::channel(None);
        let forward = Arc::new(ActiveForward {
            htl: request.htl,
            attempt_budget,
            result,
        });
        active.insert(request.hash, forward.clone());
        ForwardOwnership::Owner(ForwardOwnerGuard {
            active: self.active.clone(),
            hash: request.hash,
            forward,
            completed: false,
        })
    }

    async fn wait_for_forward(
        forward: Arc<ActiveForward>,
        context: Option<BlobRouteContext>,
    ) -> Result<BlobReply, StoreError> {
        let mut result = forward.result.subscribe();
        let wait = async {
            loop {
                if let Some(result) = result.borrow().clone() {
                    return result.map_err(StoreError::Other);
                }
                result.changed().await.map_err(|_| {
                    StoreError::Other("mesh forwarding owner closed without a result".to_string())
                })?;
            }
        };
        if let Some(context) = context {
            tokio::time::timeout_at(tokio::time::Instant::from_std(context.deadline), wait)
                .await
                .map_err(|_| {
                    StoreError::Other(
                        "coalesced mesh forwarding deadline expired before completion".to_string(),
                    )
                })?
        } else {
            wait.await
        }
    }

    async fn route_inner(
        &self,
        request: BlobRequest,
        context: Option<BlobRouteContext>,
    ) -> Result<BlobReply, StoreError> {
        let Some(forwarded) = Self::forwarded_request(request) else {
            return Ok(BlobReply::NoResult);
        };
        match self.claim_forward(request, context) {
            ForwardOwnership::SuppressCycle => Ok(BlobReply::NoResult),
            ForwardOwnership::Wait(forward) => Self::wait_for_forward(forward, context).await,
            ForwardOwnership::Untracked => match context {
                Some(context) => self.inner.route_with_context(forwarded, context).await,
                None => self.inner.route(forwarded).await,
            },
            ForwardOwnership::Owner(owner) => {
                let result = match context {
                    Some(context) => self.inner.route_with_context(forwarded, context).await,
                    None => self.inner.route(forwarded).await,
                };
                owner.complete(&result);
                result
            }
        }
    }
}

#[async_trait]
impl BlobRoute for MeshForwardingRoute {
    async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError> {
        self.route_inner(request, None).await
    }

    async fn route_with_context(
        &self,
        request: BlobRequest,
        context: BlobRouteContext,
    ) -> Result<BlobReply, StoreError> {
        self.route_inner(request, Some(context)).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    use super::*;
    use hashtree_core::sha256;

    #[derive(Default)]
    struct RecordingRoute {
        requests: Mutex<Vec<BlobRequest>>,
        contexts: Mutex<Vec<BlobRouteContext>>,
    }

    #[derive(Default)]
    struct CycleRoute {
        next: OnceLock<Arc<dyn BlobRoute>>,
        requests: Mutex<Vec<BlobRequest>>,
    }

    struct SlowRoute {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl BlobRoute for SlowRoute {
        async fn route(&self, _request: BlobRequest) -> Result<BlobReply, StoreError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok(BlobReply::NoResult)
        }
    }

    #[async_trait]
    impl BlobRoute for CycleRoute {
        async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError> {
            self.requests.lock().unwrap().push(request);
            self.next.get().unwrap().route(request).await
        }
    }

    #[async_trait]
    impl BlobRoute for RecordingRoute {
        async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError> {
            self.requests.lock().unwrap().push(request);
            Ok(BlobReply::NoResult)
        }

        async fn route_with_context(
            &self,
            request: BlobRequest,
            context: BlobRouteContext,
        ) -> Result<BlobReply, StoreError> {
            self.requests.lock().unwrap().push(request);
            self.contexts.lock().unwrap().push(context);
            Ok(BlobReply::NoResult)
        }
    }

    #[tokio::test]
    async fn one_mesh_decision_consumes_exactly_one_hop() {
        let inner = Arc::new(RecordingRoute::default());
        let route = MeshForwardingRoute::new(inner.clone());
        let hash = sha256(b"one hop");

        assert_eq!(
            route.route(BlobRequest { hash, htl: 2 }).await.unwrap(),
            BlobReply::NoResult
        );
        assert_eq!(
            inner.requests.lock().unwrap().as_slice(),
            &[BlobRequest { hash, htl: 1 }]
        );
    }

    #[tokio::test]
    async fn exhausted_request_is_route_local_no_result() {
        let inner = Arc::new(RecordingRoute::default());
        let route = MeshForwardingRoute::new(inner.clone());
        let hash = sha256(b"exhausted");

        assert_eq!(
            route.route(BlobRequest { hash, htl: 0 }).await.unwrap(),
            BlobReply::NoResult
        );
        assert!(inner.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn nested_mesh_decisions_observe_two_one_zero() {
        let terminal = Arc::new(RecordingRoute::default());
        let second: Arc<dyn BlobRoute> = Arc::new(MeshForwardingRoute::new(terminal.clone()));
        let first = MeshForwardingRoute::new(second);
        let hash = sha256(b"two hops");

        assert_eq!(
            first.route(BlobRequest { hash, htl: 2 }).await.unwrap(),
            BlobReply::NoResult
        );
        assert_eq!(
            terminal.requests.lock().unwrap().as_slice(),
            &[BlobRequest { hash, htl: 0 }]
        );
    }

    #[tokio::test]
    async fn forwarding_preserves_deadline_and_attempt_budget() {
        let inner = Arc::new(RecordingRoute::default());
        let route = MeshForwardingRoute::new(inner.clone());
        let hash = sha256(b"context");
        let context = BlobRouteContext {
            deadline: Instant::now() + Duration::from_secs(3),
            attempt_budget: 2,
        };

        route
            .route_with_context(BlobRequest { hash, htl: 1 }, context)
            .await
            .unwrap();
        assert_eq!(
            inner.requests.lock().unwrap().as_slice(),
            &[BlobRequest { hash, htl: 0 }]
        );
        let observed = inner.contexts.lock().unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].deadline, context.deadline);
        assert_eq!(observed[0].attempt_budget, context.attempt_budget);
    }

    #[tokio::test]
    async fn lower_htl_cycle_reentry_is_suppressed_before_repeating_provider_work() {
        let first = Arc::new(CycleRoute::default());
        let second = Arc::new(CycleRoute::default());
        let first_forwarder: Arc<dyn BlobRoute> = Arc::new(MeshForwardingRoute::new(first.clone()));
        let second_forwarder: Arc<dyn BlobRoute> =
            Arc::new(MeshForwardingRoute::new(second.clone()));
        assert!(first.next.set(second_forwarder.clone()).is_ok());
        assert!(second.next.set(first_forwarder.clone()).is_ok());
        let hash = sha256(b"cycle");

        assert_eq!(
            first_forwarder
                .route(BlobRequest { hash, htl: 3 })
                .await
                .unwrap(),
            BlobReply::NoResult
        );
        assert_eq!(
            first.requests.lock().unwrap().as_slice(),
            &[BlobRequest { hash, htl: 2 }]
        );
        assert_eq!(
            second.requests.lock().unwrap().as_slice(),
            &[BlobRequest { hash, htl: 1 }]
        );
    }

    #[tokio::test]
    async fn equal_in_flight_requests_share_one_mesh_attempt() {
        let inner = Arc::new(SlowRoute {
            calls: AtomicUsize::new(0),
        });
        let route = Arc::new(MeshForwardingRoute::new(inner.clone()));
        let request = BlobRequest {
            hash: sha256(b"duplicate"),
            htl: 2,
        };

        let (first, second) = tokio::join!(route.route(request), route.route(request));
        assert_eq!(first.unwrap(), BlobReply::NoResult);
        assert_eq!(second.unwrap(), BlobReply::NoResult);
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
    }
}
