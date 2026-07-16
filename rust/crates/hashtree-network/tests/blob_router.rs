use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hashtree_core::{
    sha256, BlobReply, BlobRequest, BlobRoute, BlobRouteContext, MemoryStore, Store, StoreError,
};
use hashtree_network::{BlobRouteEntry, BlobRouteIdentity, BlobRouter, BlobRouterConfig};
use tokio::sync::Mutex;

#[derive(Clone)]
enum PlannedReply {
    Data(Vec<u8>),
    NoResult,
    Failure,
}

struct ProbeRoute {
    id: &'static str,
    replies: Mutex<VecDeque<PlannedReply>>,
    fallback: PlannedReply,
    delay: Duration,
    calls: AtomicUsize,
    order: Arc<StdMutex<Vec<&'static str>>>,
    contexts: StdMutex<Vec<BlobRouteContext>>,
    request_htls: StdMutex<Vec<u8>>,
    active: Option<Arc<AtomicUsize>>,
    max_active: Option<Arc<AtomicUsize>>,
}

impl ProbeRoute {
    fn new(
        id: &'static str,
        reply: PlannedReply,
        delay: Duration,
        order: Arc<StdMutex<Vec<&'static str>>>,
    ) -> Self {
        Self {
            id,
            replies: Mutex::new(VecDeque::new()),
            fallback: reply,
            delay,
            calls: AtomicUsize::new(0),
            order,
            contexts: StdMutex::new(Vec::new()),
            request_htls: StdMutex::new(Vec::new()),
            active: None,
            max_active: None,
        }
    }

    fn with_concurrency(mut self, active: Arc<AtomicUsize>, max_active: Arc<AtomicUsize>) -> Self {
        self.active = Some(active);
        self.max_active = Some(max_active);
        self
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl BlobRoute for ProbeRoute {
    async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError> {
        self.route_with_context(
            request,
            BlobRouteContext {
                deadline: Instant::now() + Duration::from_secs(1),
                attempt_budget: usize::MAX,
            },
        )
        .await
    }

    async fn route_with_context(
        &self,
        request: BlobRequest,
        context: BlobRouteContext,
    ) -> Result<BlobReply, StoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.order.lock().unwrap().push(self.id);
        self.contexts.lock().unwrap().push(context);
        self.request_htls.lock().unwrap().push(request.htl);
        let active_guard = self.active.as_ref().map(|active| {
            let count = active.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some(max_active) = &self.max_active {
                max_active.fetch_max(count, Ordering::SeqCst);
            }
            ActiveGuard(Arc::clone(active))
        });
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let reply = self
            .replies
            .lock()
            .await
            .pop_front()
            .unwrap_or_else(|| self.fallback.clone());
        drop(active_guard);
        match reply {
            PlannedReply::Data(data) => Ok(BlobReply::Data(data)),
            PlannedReply::NoResult => Ok(BlobReply::NoResult),
            PlannedReply::Failure => Err(StoreError::Other("transport failed".to_string())),
        }
    }
}

struct ActiveGuard(Arc<AtomicUsize>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn config() -> BlobRouterConfig {
    BlobRouterConfig {
        request_timeout: Duration::from_millis(250),
        max_routes: 8,
        max_route_attempts: 8,
        route_attempt_budget: 3,
        max_in_flight: 1,
        hedge_delay: Duration::from_millis(5),
        initial_cooldown: Duration::from_millis(100),
        max_cooldown: Duration::from_millis(200),
        exploration_interval: 16,
    }
}

fn entry(id: &str, route: Arc<dyn BlobRoute>) -> BlobRouteEntry {
    BlobRouteEntry::new(id, route)
}

#[tokio::test]
async fn explicit_preference_is_honored_and_no_result_stays_route_local() {
    let data = b"preferred route fallback".to_vec();
    let hash = sha256(&data);
    let order = Arc::new(StdMutex::new(Vec::new()));
    let miss = Arc::new(ProbeRoute::new(
        "miss",
        PlannedReply::NoResult,
        Duration::ZERO,
        Arc::clone(&order),
    ));
    let hit = Arc::new(ProbeRoute::new(
        "hit",
        PlannedReply::Data(data.clone()),
        Duration::ZERO,
        Arc::clone(&order),
    ));
    let router = BlobRouter::new(
        vec![entry("hit", hit.clone()), entry("miss", miss.clone())],
        None,
        config(),
    )
    .unwrap();

    assert_eq!(
        router
            .get(&hash, Some(&[BlobRouteIdentity::from("miss")]))
            .await
            .unwrap(),
        Some(data)
    );
    assert_eq!(&*order.lock().unwrap(), &["miss", "hit"]);
    assert_eq!(miss.calls(), 1);
    assert_eq!(hit.calls(), 1);
    let outcomes = router.outcomes().await;
    assert_eq!(
        outcomes[&BlobRouteIdentity::from("miss")].failure_weight,
        0.0
    );
}

#[tokio::test]
async fn successful_latency_adapts_order_without_overriding_preferences() {
    let data = b"adaptive latency".to_vec();
    let hash = sha256(&data);
    let order = Arc::new(StdMutex::new(Vec::new()));
    let slow = Arc::new(ProbeRoute::new(
        "slow",
        PlannedReply::Data(data.clone()),
        Duration::from_millis(20),
        Arc::clone(&order),
    ));
    let fast = Arc::new(ProbeRoute::new(
        "fast",
        PlannedReply::Data(data.clone()),
        Duration::from_millis(1),
        Arc::clone(&order),
    ));
    let router = BlobRouter::new(
        vec![entry("slow", slow.clone()), entry("fast", fast.clone())],
        None,
        config(),
    )
    .unwrap();

    router
        .get(&hash, Some(&[BlobRouteIdentity::from("slow")]))
        .await
        .unwrap();
    router
        .get(&hash, Some(&[BlobRouteIdentity::from("fast")]))
        .await
        .unwrap();
    let outcomes = router.outcomes().await;
    assert!(
        outcomes[&BlobRouteIdentity::from("fast")]
            .successful_latency_ms
            .unwrap()
            < outcomes[&BlobRouteIdentity::from("slow")]
                .successful_latency_ms
                .unwrap()
    );
    order.lock().unwrap().clear();
    router.get(&hash, None).await.unwrap();
    assert_eq!(&*order.lock().unwrap(), &["fast"]);

    order.lock().unwrap().clear();
    router
        .get(&hash, Some(&[BlobRouteIdentity::from("slow")]))
        .await
        .unwrap();
    assert_eq!(&*order.lock().unwrap(), &["slow"]);
}

#[tokio::test]
async fn stale_routes_are_periodically_explored() {
    let data = b"bounded exploration".to_vec();
    let hash = sha256(&data);
    let order = Arc::new(StdMutex::new(Vec::new()));
    let preferred = Arc::new(ProbeRoute::new(
        "preferred",
        PlannedReply::Data(data.clone()),
        Duration::from_millis(1),
        Arc::clone(&order),
    ));
    let stale = Arc::new(ProbeRoute::new(
        "stale",
        PlannedReply::Data(data.clone()),
        Duration::from_millis(15),
        Arc::clone(&order),
    ));
    let router = BlobRouter::new(
        vec![entry("preferred", preferred), entry("stale", stale)],
        None,
        BlobRouterConfig {
            exploration_interval: 4,
            ..config()
        },
    )
    .unwrap();

    router
        .get(&hash, Some(&[BlobRouteIdentity::from("preferred")]))
        .await
        .unwrap();
    router
        .get(&hash, Some(&[BlobRouteIdentity::from("stale")]))
        .await
        .unwrap();
    router.get(&hash, None).await.unwrap();
    order.lock().unwrap().clear();
    router.get(&hash, None).await.unwrap();
    assert_eq!(&*order.lock().unwrap(), &["stale"]);
}

#[tokio::test]
async fn provider_replacement_clears_cooldown_and_recovers() {
    let data = b"provider replacement".to_vec();
    let hash = sha256(&data);
    let order = Arc::new(StdMutex::new(Vec::new()));
    let dead = Arc::new(ProbeRoute::new(
        "provider",
        PlannedReply::Failure,
        Duration::ZERO,
        Arc::clone(&order),
    ));
    let miss = Arc::new(ProbeRoute::new(
        "miss",
        PlannedReply::NoResult,
        Duration::ZERO,
        Arc::clone(&order),
    ));
    let router = BlobRouter::new(
        vec![entry("provider", dead), entry("miss", miss.clone())],
        None,
        config(),
    )
    .unwrap();
    assert!(router
        .get(&hash, Some(&[BlobRouteIdentity::from("provider")]))
        .await
        .is_err());
    assert!(router.outcomes().await[&BlobRouteIdentity::from("provider")].cooling_down);

    let replacement = Arc::new(ProbeRoute::new(
        "replacement",
        PlannedReply::Data(data.clone()),
        Duration::ZERO,
        Arc::clone(&order),
    ));
    router
        .set_routes(vec![entry("provider", replacement), entry("miss", miss)])
        .await
        .unwrap();
    assert!(!router.outcomes().await[&BlobRouteIdentity::from("provider")].cooling_down);
    assert_eq!(
        router
            .get(&hash, Some(&[BlobRouteIdentity::from("provider")]))
            .await
            .unwrap(),
        Some(data)
    );
}

#[tokio::test]
async fn corrupt_responses_are_not_returned_or_cached() {
    let data = b"verified response".to_vec();
    let hash = sha256(&data);
    let order = Arc::new(StdMutex::new(Vec::new()));
    let corrupt = Arc::new(ProbeRoute::new(
        "corrupt",
        PlannedReply::Data(b"wrong bytes".to_vec()),
        Duration::ZERO,
        Arc::clone(&order),
    ));
    let healthy = Arc::new(ProbeRoute::new(
        "healthy",
        PlannedReply::Data(data.clone()),
        Duration::ZERO,
        Arc::clone(&order),
    ));
    let cache = Arc::new(MemoryStore::new());
    let router = BlobRouter::new(
        vec![entry("corrupt", corrupt), entry("healthy", healthy)],
        Some(cache.clone()),
        config(),
    )
    .unwrap();

    assert_eq!(
        router
            .get(&hash, Some(&[BlobRouteIdentity::from("corrupt")]))
            .await
            .unwrap(),
        Some(data.clone())
    );
    assert_eq!(cache.get(&hash).await.unwrap(), Some(data));

    let bad_hash = sha256(b"missing good response");
    let all_bad = BlobRouter::new(
        vec![entry(
            "corrupt",
            Arc::new(ProbeRoute::new(
                "corrupt",
                PlannedReply::Data(b"still wrong".to_vec()),
                Duration::ZERO,
                Arc::new(StdMutex::new(Vec::new())),
            )),
        )],
        Some(cache.clone()),
        config(),
    )
    .unwrap();
    assert!(all_bad.get(&bad_hash, None).await.is_err());
    assert_eq!(cache.get(&bad_hash).await.unwrap(), None);
}

#[tokio::test]
async fn composite_route_owns_its_peers_and_receives_bounded_context() {
    let data = b"composite ownership".to_vec();
    let hash = sha256(&data);
    let order = Arc::new(StdMutex::new(Vec::new()));
    let composite = Arc::new(ProbeRoute::new(
        "composite",
        PlannedReply::Data(data.clone()),
        Duration::ZERO,
        order,
    ));
    let router = BlobRouter::new(
        vec![entry("peer-set", composite.clone())],
        None,
        BlobRouterConfig {
            route_attempt_budget: 2,
            ..config()
        },
    )
    .unwrap();

    assert_eq!(router.get(&hash, None).await.unwrap(), Some(data));
    assert_eq!(router.route_count().await, 1);
    assert_eq!(composite.calls(), 1);
    let contexts = composite.contexts.lock().unwrap();
    assert_eq!(contexts[0].attempt_budget, 2);
    assert!(contexts[0].deadline > Instant::now());
}

#[tokio::test]
async fn terminal_routes_receive_htl_unchanged() {
    let hash = sha256(b"terminal miss");
    let route = Arc::new(ProbeRoute::new(
        "terminal",
        PlannedReply::NoResult,
        Duration::ZERO,
        Arc::new(StdMutex::new(Vec::new())),
    ));
    let router = BlobRouter::new(vec![entry("terminal", route.clone())], None, config()).unwrap();

    assert_eq!(
        BlobRoute::route(&router, BlobRequest { hash, htl: 3 })
            .await
            .unwrap(),
        BlobReply::NoResult
    );
    assert_eq!(&*route.request_htls.lock().unwrap(), &[3]);
}

#[tokio::test]
async fn route_attempts_and_fanout_stay_bounded() {
    let hash = sha256(b"not present");
    let order = Arc::new(StdMutex::new(Vec::new()));
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let mut routes = Vec::new();
    for id in ["a", "b", "c", "d"] {
        routes.push(entry(
            id,
            Arc::new(
                ProbeRoute::new(
                    Box::leak(id.to_string().into_boxed_str()),
                    PlannedReply::NoResult,
                    Duration::from_millis(20),
                    Arc::clone(&order),
                )
                .with_concurrency(Arc::clone(&active), Arc::clone(&max_active)),
            ),
        ));
    }
    let router = BlobRouter::new(
        routes,
        None,
        BlobRouterConfig {
            max_route_attempts: 3,
            max_in_flight: 2,
            ..config()
        },
    )
    .unwrap();

    let error = router.get(&hash, None).await.unwrap_err();
    assert!(error.to_string().contains("attempt budget"));
    assert_eq!(order.lock().unwrap().len(), 3);
    assert!(max_active.load(Ordering::SeqCst) <= 2);
}
