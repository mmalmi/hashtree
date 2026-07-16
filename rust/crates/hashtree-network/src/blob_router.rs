//! Adaptive routing across opaque blob sources.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use hashtree_core::{
    BlobReply, BlobRequest, BlobRoute, BlobRouteContext, Hash, Store, StoreError, BLOB_DEFAULT_HTL,
    BLOB_MAX_BYTES, BLOB_MAX_HTL,
};
use tokio::sync::{Mutex, RwLock};

const OUTCOME_DECAY: f64 = 0.875;

/// Stable, application-owned identity for one opaque route.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BlobRouteIdentity(String);

impl BlobRouteIdentity {
    pub fn new(identity: impl Into<String>) -> Self {
        Self(identity.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for BlobRouteIdentity {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for BlobRouteIdentity {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// One route registered with the outer router.
#[derive(Clone)]
pub struct BlobRouteEntry {
    pub identity: BlobRouteIdentity,
    pub route: Arc<dyn BlobRoute>,
}

impl BlobRouteEntry {
    pub fn new(identity: impl Into<BlobRouteIdentity>, route: Arc<dyn BlobRoute>) -> Self {
        Self {
            identity: identity.into(),
            route,
        }
    }
}

/// Fixed bounds for one in-memory router.
#[derive(Clone, Copy, Debug)]
pub struct BlobRouterConfig {
    pub request_timeout: Duration,
    pub max_routes: usize,
    pub max_route_attempts: usize,
    pub route_attempt_budget: usize,
    pub max_in_flight: usize,
    pub hedge_delay: Duration,
    pub initial_cooldown: Duration,
    pub max_cooldown: Duration,
    pub exploration_interval: u64,
}

impl Default for BlobRouterConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(10),
            max_routes: 32,
            max_route_attempts: 32,
            route_attempt_budget: 4,
            max_in_flight: 2,
            hedge_delay: Duration::from_millis(75),
            initial_cooldown: Duration::from_millis(250),
            max_cooldown: Duration::from_secs(10),
            exploration_interval: 16,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct RouteOutcomes {
    successes: f64,
    failures: f64,
    timeouts: f64,
    successful_latency_ms: Option<f64>,
    consecutive_failures: u32,
    cooldown_until: Option<Instant>,
    last_attempt: Option<Instant>,
}

impl RouteOutcomes {
    fn decay(&mut self) {
        self.successes *= OUTCOME_DECAY;
        self.failures *= OUTCOME_DECAY;
        self.timeouts *= OUTCOME_DECAY;
    }

    fn reliability(&self) -> f64 {
        (self.successes + 1.0) / (self.successes + self.failures + self.timeouts + 2.0)
    }

    fn score(&self) -> f64 {
        let latency = self
            .successful_latency_ms
            .map(|latency| 50.0 / (latency + 50.0))
            .unwrap_or(0.5);
        0.7 * self.reliability() + 0.3 * latency
    }
}

#[derive(Default)]
struct RouterState {
    searches: u64,
    outcomes: HashMap<BlobRouteIdentity, RouteOutcomes>,
}

/// Read-only view of bounded, decaying route outcomes.
#[derive(Clone, Debug)]
pub struct BlobRouteOutcomeSnapshot {
    pub successful_weight: f64,
    pub failure_weight: f64,
    pub timeout_weight: f64,
    pub successful_latency_ms: Option<f64>,
    pub cooling_down: bool,
}

enum AttemptOutcome {
    Data {
        identity: BlobRouteIdentity,
        data: Vec<u8>,
        elapsed: Duration,
    },
    NoResult {
        identity: BlobRouteIdentity,
    },
    Failure {
        identity: BlobRouteIdentity,
        message: String,
    },
}

type AttemptFuture = Pin<Box<dyn Future<Output = AttemptOutcome> + Send>>;

fn launch_attempt(
    ordered: &[BlobRouteEntry],
    next_route: &mut usize,
    attempted: &mut usize,
    pending: &mut FuturesUnordered<AttemptFuture>,
    pending_identities: &mut HashSet<BlobRouteIdentity>,
    request: BlobRequest,
    route_context: BlobRouteContext,
) {
    let entry = ordered[*next_route].clone();
    *next_route += 1;
    *attempted += 1;
    pending_identities.insert(entry.identity.clone());
    pending.push(
        async move {
            let started = Instant::now();
            let result = AssertUnwindSafe(entry.route.route_with_context(request, route_context))
                .catch_unwind()
                .await;
            match result {
                Ok(Ok(BlobReply::Data(data))) => AttemptOutcome::Data {
                    identity: entry.identity,
                    data,
                    elapsed: started.elapsed(),
                },
                Ok(Ok(BlobReply::NoResult)) => AttemptOutcome::NoResult {
                    identity: entry.identity,
                },
                Ok(Err(error)) => AttemptOutcome::Failure {
                    identity: entry.identity,
                    message: error.to_string(),
                },
                Err(_) => AttemptOutcome::Failure {
                    identity: entry.identity,
                    message: "route panicked".to_string(),
                },
            }
        }
        .boxed(),
    );
}

/// One adaptive read router. Routes remain opaque and own any peer sets behind
/// them; the router only orders route identities and bounds route attempts.
pub struct BlobRouter {
    routes: RwLock<Vec<BlobRouteEntry>>,
    state: Mutex<RouterState>,
    cache: Option<Arc<dyn Store>>,
    config: BlobRouterConfig,
}

impl BlobRouter {
    pub fn new(
        routes: Vec<BlobRouteEntry>,
        cache: Option<Arc<dyn Store>>,
        config: BlobRouterConfig,
    ) -> Result<Self, StoreError> {
        validate_routes(&routes, config.max_routes)?;
        let outcomes = routes
            .iter()
            .map(|entry| (entry.identity.clone(), RouteOutcomes::default()))
            .collect();
        Ok(Self {
            routes: RwLock::new(routes),
            state: Mutex::new(RouterState {
                searches: 0,
                outcomes,
            }),
            cache,
            config,
        })
    }

    /// Replace the route set. Outcome state survives only when both identity
    /// and the exact route allocation are unchanged; provider replacement is
    /// therefore immediately eligible for recovery.
    pub async fn set_routes(&self, routes: Vec<BlobRouteEntry>) -> Result<(), StoreError> {
        validate_routes(&routes, self.config.max_routes)?;
        let mut current = self.routes.write().await;
        let old_routes: HashMap<_, _> = current
            .iter()
            .map(|entry| (entry.identity.clone(), Arc::clone(&entry.route)))
            .collect();
        let mut state = self.state.lock().await;
        state.outcomes.retain(|identity, _| {
            routes.iter().any(|entry| {
                &entry.identity == identity
                    && old_routes
                        .get(identity)
                        .is_some_and(|old| Arc::ptr_eq(old, &entry.route))
            })
        });
        for entry in &routes {
            state.outcomes.entry(entry.identity.clone()).or_default();
        }
        *current = routes;
        Ok(())
    }

    pub async fn route_count(&self) -> usize {
        self.routes.read().await.len()
    }

    pub async fn outcomes(&self) -> HashMap<BlobRouteIdentity, BlobRouteOutcomeSnapshot> {
        let now = Instant::now();
        self.state
            .lock()
            .await
            .outcomes
            .iter()
            .map(|(identity, outcome)| {
                (
                    identity.clone(),
                    BlobRouteOutcomeSnapshot {
                        successful_weight: outcome.successes,
                        failure_weight: outcome.failures,
                        timeout_weight: outcome.timeouts,
                        successful_latency_ms: outcome.successful_latency_ms,
                        cooling_down: outcome.cooldown_until.is_some_and(|until| until > now),
                    },
                )
            })
            .collect()
    }

    pub async fn get(
        &self,
        hash: &Hash,
        preferred: Option<&[BlobRouteIdentity]>,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.get_request(
            BlobRequest {
                hash: *hash,
                htl: BLOB_DEFAULT_HTL,
            },
            preferred,
            None,
        )
        .await
    }

    pub async fn get_request(
        &self,
        request: BlobRequest,
        preferred: Option<&[BlobRouteIdentity]>,
        outer_context: Option<BlobRouteContext>,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        if request.htl > BLOB_MAX_HTL {
            return Err(StoreError::Other(format!(
                "Hashtree blob HTL {} exceeds the maximum of {BLOB_MAX_HTL}",
                request.htl
            )));
        }

        let now = Instant::now();
        let own_deadline = now + self.config.request_timeout;
        let deadline = outer_context
            .map(|context| context.deadline.min(own_deadline))
            .unwrap_or(own_deadline);
        if deadline <= now {
            return Err(StoreError::Other(
                "blob retrieval deadline expired before the search started".to_string(),
            ));
        }
        let outer_budget = outer_context
            .map(|context| context.attempt_budget)
            .unwrap_or(self.config.max_route_attempts);
        let max_attempts = self.config.max_route_attempts.min(outer_budget);
        if max_attempts == 0 {
            return Err(StoreError::Other(
                "blob retrieval has no route attempt budget".to_string(),
            ));
        }

        let ordered = self.ordered_routes(preferred).await;
        if ordered.is_empty() {
            return Ok(None);
        }
        let max_attempts = max_attempts.min(ordered.len());
        let max_in_flight = self.config.max_in_flight.max(1).min(max_attempts);
        let route_context = BlobRouteContext {
            deadline,
            attempt_budget: self.config.route_attempt_budget.min(outer_budget).max(1),
        };

        let mut pending: FuturesUnordered<AttemptFuture> = FuturesUnordered::new();
        let mut pending_identities = HashSet::new();
        let mut next_route = 0usize;
        let mut attempted = 0usize;
        let mut failures = Vec::new();
        launch_attempt(
            &ordered,
            &mut next_route,
            &mut attempted,
            &mut pending,
            &mut pending_identities,
            request,
            route_context,
        );
        let mut next_hedge = Instant::now() + self.config.hedge_delay;
        let deadline_sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
        tokio::pin!(deadline_sleep);

        loop {
            if pending.is_empty() {
                if attempted < max_attempts {
                    launch_attempt(
                        &ordered,
                        &mut next_route,
                        &mut attempted,
                        &mut pending,
                        &mut pending_identities,
                        request,
                        route_context,
                    );
                    next_hedge = Instant::now() + self.config.hedge_delay;
                } else {
                    break;
                }
            }

            let hedge_at = tokio::time::Instant::from_std(next_hedge.min(deadline));
            tokio::select! {
                _ = &mut deadline_sleep => {
                    for identity in pending_identities.drain() {
                        self.record_timeout(&identity).await;
                    }
                    return Err(StoreError::Other(
                        "blob retrieval deadline expired before the search completed".to_string(),
                    ));
                }
                outcome = pending.next() => {
                    let Some(outcome) = outcome else { continue };
                    let identity = match &outcome {
                        AttemptOutcome::Data { identity, .. }
                        | AttemptOutcome::NoResult { identity }
                        | AttemptOutcome::Failure { identity, .. } => identity.clone(),
                    };
                    pending_identities.remove(&identity);
                    match outcome {
                        AttemptOutcome::Data { identity, data, elapsed } => {
                            if data.len() > BLOB_MAX_BYTES {
                                let message = format!(
                                    "route {} returned {} bytes, exceeding the {BLOB_MAX_BYTES}-byte limit",
                                    identity.as_str(),
                                    data.len()
                                );
                                self.record_failure(&identity).await;
                                failures.push(message);
                            } else if hashtree_core::sha256(&data) != request.hash {
                                let message = format!(
                                    "route {} returned content with the wrong hash",
                                    identity.as_str()
                                );
                                self.record_failure(&identity).await;
                                failures.push(message);
                            } else {
                                self.record_success(&identity, elapsed).await;
                                if let Some(cache) = &self.cache {
                                    let _ = cache.put(request.hash, data.clone()).await;
                                }
                                return Ok(Some(data));
                            }
                        }
                        AttemptOutcome::NoResult { identity } => {
                            self.record_no_result(&identity).await;
                        }
                        AttemptOutcome::Failure { identity, message } => {
                            self.record_failure(&identity).await;
                            failures.push(format!("route {}: {message}", identity.as_str()));
                        }
                    }
                    if attempted < max_attempts && pending.len() < max_in_flight {
                        launch_attempt(
                            &ordered,
                            &mut next_route,
                            &mut attempted,
                            &mut pending,
                            &mut pending_identities,
                            request,
                            route_context,
                        );
                        next_hedge = Instant::now() + self.config.hedge_delay;
                    }
                }
                _ = tokio::time::sleep_until(hedge_at), if attempted < max_attempts && pending.len() < max_in_flight => {
                    launch_attempt(
                        &ordered,
                        &mut next_route,
                        &mut attempted,
                        &mut pending,
                        &mut pending_identities,
                        request,
                        route_context,
                    );
                    next_hedge = Instant::now() + self.config.hedge_delay;
                }
            }
        }

        if attempted < ordered.len() {
            return Err(StoreError::Other(format!(
                "blob retrieval exhausted its bounded route attempt budget ({attempted}/{})",
                ordered.len()
            )));
        }
        if let Some(message) = failures.into_iter().next() {
            return Err(StoreError::Other(format!(
                "blob retrieval was incomplete: {message}"
            )));
        }
        Ok(None)
    }

    async fn ordered_routes(&self, preferred: Option<&[BlobRouteIdentity]>) -> Vec<BlobRouteEntry> {
        let routes = self.routes.read().await.clone();
        let preferred_order: HashMap<_, _> = preferred
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(index, identity)| (identity.clone(), index))
            .collect();
        let now = Instant::now();
        let mut state = self.state.lock().await;
        state.searches = state.searches.wrapping_add(1);
        let explore = self.config.exploration_interval > 0
            && state
                .searches
                .is_multiple_of(self.config.exploration_interval);
        let mut ordered = routes;
        ordered.sort_by(|left, right| {
            match (
                preferred_order.get(&left.identity),
                preferred_order.get(&right.identity),
            ) {
                (Some(left), Some(right)) => return left.cmp(right),
                (Some(_), None) => return std::cmp::Ordering::Less,
                (None, Some(_)) => return std::cmp::Ordering::Greater,
                (None, None) => {}
            }
            let left_stats = state
                .outcomes
                .get(&left.identity)
                .cloned()
                .unwrap_or_default();
            let right_stats = state
                .outcomes
                .get(&right.identity)
                .cloned()
                .unwrap_or_default();
            if explore {
                return left_stats
                    .last_attempt
                    .cmp(&right_stats.last_attempt)
                    .then_with(|| left.identity.cmp(&right.identity));
            }
            let left_cooling = left_stats.cooldown_until.is_some_and(|until| until > now);
            let right_cooling = right_stats.cooldown_until.is_some_and(|until| until > now);
            left_cooling
                .cmp(&right_cooling)
                .then_with(|| {
                    right_stats
                        .score()
                        .partial_cmp(&left_stats.score())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| left_stats.last_attempt.cmp(&right_stats.last_attempt))
                .then_with(|| left.identity.cmp(&right.identity))
        });
        ordered
    }

    async fn record_success(&self, identity: &BlobRouteIdentity, elapsed: Duration) {
        let mut state = self.state.lock().await;
        let outcome = state.outcomes.entry(identity.clone()).or_default();
        outcome.decay();
        outcome.successes += 1.0;
        let elapsed_ms = elapsed.as_secs_f64() * 1_000.0;
        outcome.successful_latency_ms = Some(
            outcome
                .successful_latency_ms
                .map(|old| 0.875 * old + 0.125 * elapsed_ms)
                .unwrap_or(elapsed_ms),
        );
        outcome.consecutive_failures = 0;
        outcome.cooldown_until = None;
        outcome.last_attempt = Some(Instant::now());
    }

    async fn record_no_result(&self, identity: &BlobRouteIdentity) {
        let mut state = self.state.lock().await;
        let outcome = state.outcomes.entry(identity.clone()).or_default();
        outcome.decay();
        outcome.consecutive_failures = 0;
        outcome.cooldown_until = None;
        outcome.last_attempt = Some(Instant::now());
    }

    async fn record_failure(&self, identity: &BlobRouteIdentity) {
        let mut state = self.state.lock().await;
        let outcome = state.outcomes.entry(identity.clone()).or_default();
        outcome.decay();
        outcome.failures += 1.0;
        self.apply_cooldown(outcome);
    }

    async fn record_timeout(&self, identity: &BlobRouteIdentity) {
        let mut state = self.state.lock().await;
        let outcome = state.outcomes.entry(identity.clone()).or_default();
        outcome.decay();
        outcome.timeouts += 1.0;
        self.apply_cooldown(outcome);
    }

    fn apply_cooldown(&self, outcome: &mut RouteOutcomes) {
        outcome.consecutive_failures = outcome.consecutive_failures.saturating_add(1);
        let multiplier =
            2u32.saturating_pow(outcome.consecutive_failures.saturating_sub(1).min(16));
        let cooldown = self
            .config
            .initial_cooldown
            .saturating_mul(multiplier)
            .min(self.config.max_cooldown);
        let now = Instant::now();
        outcome.cooldown_until = Some(now + cooldown);
        outcome.last_attempt = Some(now);
    }
}

#[async_trait]
impl BlobRoute for BlobRouter {
    async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError> {
        Ok(match self.get_request(request, None, None).await? {
            Some(data) => BlobReply::Data(data),
            None => BlobReply::NoResult,
        })
    }

    async fn route_with_context(
        &self,
        request: BlobRequest,
        context: BlobRouteContext,
    ) -> Result<BlobReply, StoreError> {
        Ok(
            match self.get_request(request, None, Some(context)).await? {
                Some(data) => BlobReply::Data(data),
                None => BlobReply::NoResult,
            },
        )
    }
}

fn validate_routes(routes: &[BlobRouteEntry], max_routes: usize) -> Result<(), StoreError> {
    if routes.len() > max_routes {
        return Err(StoreError::Other(format!(
            "blob router has {} routes, exceeding its bound of {max_routes}",
            routes.len()
        )));
    }
    let mut identities = HashSet::with_capacity(routes.len());
    for entry in routes {
        if entry.identity.as_str().is_empty() {
            return Err(StoreError::Other(
                "blob route identity must not be empty".to_string(),
            ));
        }
        if !identities.insert(entry.identity.clone()) {
            return Err(StoreError::Other(format!(
                "duplicate blob route identity {}",
                entry.identity.as_str()
            )));
        }
    }
    Ok(())
}
