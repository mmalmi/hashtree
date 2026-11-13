//! The cost tier: a route that costs money is asked only after every free
//! route of the search has finished without data, and never as a hedge.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hashtree_core::{sha256, BlobReply, BlobRequest, BlobRoute, Hash, StoreError};
use hashtree_network::{
    BlobRouteCost, BlobRouteEntry, BlobRouteIdentity, BlobRouter, BlobRouterConfig,
};

#[derive(Clone)]
enum Answer {
    Data(Vec<u8>),
    NoResult,
    Failure,
}

/// One start or end of an attempt, in the order the router caused them.
type Log = Arc<Mutex<Vec<(&'static str, &'static str)>>>;

struct Probe {
    id: &'static str,
    answer: Answer,
    delay: Duration,
    calls: AtomicUsize,
    log: Log,
}

impl Probe {
    fn new(id: &'static str, answer: Answer, delay: Duration, log: &Log) -> Arc<Self> {
        Arc::new(Self {
            id,
            answer,
            delay,
            calls: AtomicUsize::new(0),
            log: Arc::clone(log),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl BlobRoute for Probe {
    async fn route(&self, _request: BlobRequest) -> Result<BlobReply, StoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.log.lock().unwrap().push((self.id, "start"));
        tokio::time::sleep(self.delay).await;
        self.log.lock().unwrap().push((self.id, "end"));
        match self.answer.clone() {
            Answer::Data(data) => Ok(BlobReply::Data(data)),
            Answer::NoResult => Ok(BlobReply::NoResult),
            Answer::Failure => Err(StoreError::Other("transport failed".to_string())),
        }
    }
}

const SHORT: Duration = Duration::from_millis(30);

/// Hedges at once and allows three attempts in flight, so only the cost
/// tier keeps a paid route waiting.
fn config() -> BlobRouterConfig {
    BlobRouterConfig {
        request_timeout: Duration::from_millis(400),
        max_routes: 8,
        max_route_attempts: 8,
        route_attempt_budget: 3,
        max_in_flight: 3,
        hedge_delay: Duration::from_millis(1),
        initial_cooldown: Duration::from_millis(100),
        max_cooldown: Duration::from_millis(200),
        exploration_interval: 16,
    }
}

fn blob(text: &str) -> (Hash, Vec<u8>) {
    (sha256(text.as_bytes()), text.as_bytes().to_vec())
}

fn router(routes: Vec<BlobRouteEntry>) -> BlobRouter {
    BlobRouter::new(routes, None, config()).unwrap()
}

#[tokio::test]
async fn a_paid_route_waits_until_every_free_route_finished_without_data() {
    let (hash, data) = blob("sold only by the paid provider");
    let log = Log::default();
    let miss = Probe::new("miss", Answer::NoResult, SHORT, &log);
    let broken = Probe::new("broken", Answer::Failure, SHORT * 2, &log);
    let paid = Probe::new("paid", Answer::Data(data.clone()), Duration::ZERO, &log);
    // Registered first and preferred: neither moves it ahead of a free route.
    let router = router(vec![
        BlobRouteEntry::paid("paid", paid.clone()),
        BlobRouteEntry::new("miss", miss.clone()),
        BlobRouteEntry::new("broken", broken.clone()),
    ]);

    let preferred = [BlobRouteIdentity::from("paid")];
    let found = router.get(&hash, Some(&preferred)).await.unwrap();

    assert_eq!(found, Some(data));
    assert_eq!((miss.calls(), broken.calls(), paid.calls()), (1, 1, 1));
    let log = log.lock().unwrap();
    let paid_start = log.iter().position(|event| *event == ("paid", "start"));
    let last_free_end = log
        .iter()
        .rposition(|(id, what)| *id != "paid" && *what == "end");
    assert!(
        paid_start > last_free_end,
        "the paid route started while a free one was in flight: {log:?}"
    );
}

#[tokio::test]
async fn a_slow_free_route_is_not_hedged_with_a_paid_one() {
    let (hash, data) = blob("free, only slow");
    let log = Log::default();
    // Far longer than the hedge delay, well inside the deadline.
    let slow = Probe::new("slow", Answer::Data(data.clone()), SHORT * 4, &log);
    let paid = Probe::new("paid", Answer::Data(data.clone()), Duration::ZERO, &log);
    let router = router(vec![
        BlobRouteEntry::paid("paid", paid.clone()),
        BlobRouteEntry::new("slow", slow.clone()),
    ]);

    assert_eq!(router.get(&hash, None).await.unwrap(), Some(data));
    assert_eq!(paid.calls(), 0);
}

#[tokio::test]
async fn a_free_route_that_outlives_the_deadline_is_a_timeout_and_nothing_is_paid() {
    let (hash, data) = blob("free, slower than the deadline");
    let log = Log::default();
    let stuck = Probe::new("stuck", Answer::Data(data.clone()), SHORT * 40, &log);
    let paid = Probe::new("paid", Answer::Data(data), Duration::ZERO, &log);
    let router = router(vec![
        BlobRouteEntry::new("stuck", stuck.clone()),
        BlobRouteEntry::paid("paid", paid.clone()),
    ]);

    let error = router.get(&hash, None).await.unwrap_err();

    assert!(error.to_string().contains("deadline"), "{error}");
    assert_eq!(paid.calls(), 0);
    let outcomes = router.outcomes().await;
    assert_eq!(
        outcomes[&BlobRouteIdentity::from("stuck")].timeout_weight,
        1.0
    );
    assert_eq!(
        outcomes[&BlobRouteIdentity::from("paid")].timeout_weight,
        0.0
    );
}

#[tokio::test]
async fn a_paid_route_as_the_only_route_is_asked_at_once() {
    let (hash, data) = blob("nothing free to wait for");
    let log = Log::default();
    let paid = Probe::new("paid", Answer::Data(data.clone()), Duration::ZERO, &log);
    let router = router(vec![BlobRouteEntry::paid("paid", paid.clone())]);

    let started = Instant::now();
    assert_eq!(router.get(&hash, None).await.unwrap(), Some(data));
    assert!(started.elapsed() < SHORT * 4);
    assert_eq!(paid.calls(), 1);
    let outcomes = router.outcomes().await;
    assert_eq!(
        outcomes[&BlobRouteIdentity::from("paid")].successful_weight,
        1.0
    );
}

#[tokio::test]
async fn paid_routes_are_asked_one_at_a_time() {
    let (hash, data) = blob("second paid provider has it");
    let log = Log::default();
    let first = Probe::new("paid-a", Answer::NoResult, SHORT * 2, &log);
    let second = Probe::new("paid-b", Answer::Data(data.clone()), Duration::ZERO, &log);
    let router = router(vec![
        BlobRouteEntry::paid("paid-a", first.clone()),
        BlobRouteEntry::paid("paid-b", second.clone()),
    ]);

    assert_eq!(router.get(&hash, None).await.unwrap(), Some(data));
    let log = log.lock().unwrap();
    assert_eq!(
        *log,
        [
            ("paid-a", "start"),
            ("paid-a", "end"),
            ("paid-b", "start"),
            ("paid-b", "end")
        ]
    );
}

#[tokio::test]
async fn a_free_only_search_never_asks_a_paid_route() {
    let (hash, data) = blob("not worth the operator's money");
    let log = Log::default();
    let miss = Probe::new("miss", Answer::NoResult, Duration::ZERO, &log);
    let paid = Probe::new("paid", Answer::Data(data.clone()), Duration::ZERO, &log);
    let router = router(vec![
        BlobRouteEntry::new("miss", miss.clone()),
        BlobRouteEntry::paid("paid", paid.clone()),
    ]);
    let request = BlobRequest { hash, htl: 0 };

    let free = router
        .get_request_within_cost(request, None, None, BlobRouteCost::Free)
        .await
        .unwrap();
    assert_eq!(free, None);
    assert_eq!(paid.calls(), 0);

    let any = router
        .get_request_within_cost(request, None, None, BlobRouteCost::Paid)
        .await
        .unwrap();
    assert_eq!(any, Some(data));
    assert_eq!(paid.calls(), 1);
}

#[tokio::test]
async fn one_tier_can_be_searched_on_its_own() {
    let (hash, data) = blob("bought after sources outside the router missed");
    let log = Log::default();
    let free = Probe::new("free", Answer::NoResult, Duration::ZERO, &log);
    let paid = Probe::new("paid", Answer::Data(data.clone()), Duration::ZERO, &log);
    let router = router(vec![
        BlobRouteEntry::new("free", free.clone()),
        BlobRouteEntry::paid("paid", paid.clone()),
    ]);
    let request = BlobRequest { hash, htl: 0 };

    let bought = router
        .get_request_at_cost(request, None, None, BlobRouteCost::Paid)
        .await
        .unwrap();

    assert_eq!(bought, Some(data));
    assert_eq!((free.calls(), paid.calls()), (0, 1));
}
