//! Shared routed mesh store core.
//!
//! This module provides a concrete store wrapper that works with any local storage
//! backend plus any signaling transport and peer-link factory. Both production
//! and simulation (mocks) use this same code.

use async_trait::async_trait;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::hash::{Hash as _, Hasher};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Mutex, Notify, RwLock};
use tokio::time::Instant;

use hashtree_core::{
    BlobReply, BlobRequest, BlobRoute, BlobRouteContext, Hash, Store, StoreError, BLOB_MAX_BYTES,
};

use crate::peer_selector::{PeerMetadataSnapshot, PeerSelector, SelectionStrategy};
use crate::protocol::{
    create_pubsub_frame, create_pubsub_interest, create_pubsub_inventory, create_pubsub_want,
    create_quote_request, create_quote_response_available, create_quote_response_unavailable,
    create_request, create_request_with_quote, create_response, encode_pubsub_frame,
    encode_pubsub_interest, encode_pubsub_inventory, encode_pubsub_want, encode_quote_request,
    encode_quote_response, encode_request, encode_response, hash_to_key, parse_message,
    DataMessage, DataQuoteRequest, DataQuoteResponse, PubsubFrame, PubsubInterest, PubsubInventory,
    PubsubWant,
};
use crate::pubsub_strategy::{
    reciprocal_virtual_finish, select_reciprocal_outbound_job, OutboundJobCandidate,
    PeerTrafficSnapshot, PubsubCandidate, PubsubSchedulerConfig,
};
use crate::signaling::MeshRouter;
use crate::transport::{PeerLinkFactory, SignalingTransport, TransportError};
use crate::types::{
    should_forward_htl, PeerHTLConfig, SignalingMessage, TimedSeenSet, MAX_HTL, MESH_EVENT_POLICY,
};

// Keep the on-disk namespace stable across the crate rename so existing peer
// metadata does not disappear for users upgrading from the old package name.
const PEER_METADATA_POINTER_SLOT_KEY: &[u8] = b"hashtree-mesh/peer-metadata/latest/v1";
const PUBSUB_SEEN_CAPACITY: usize = 16_384;
const PUBSUB_INBOX_CAPACITY: usize = 4_096;
const PUBSUB_FRAME_CACHE_CAPACITY: usize = 4_096;
const VERIFIED_BLOCK_DELIVERY_CAPACITY: usize = 4_096;
const PUBSUB_SEEN_TTL: Duration = Duration::from_secs(120);

/// Pending request awaiting response
struct PendingRequest {
    owner: Arc<()>,
    response_tx: oneshot::Sender<Option<Vec<u8>>>,
    started_at: Instant,
    queried_peers: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct PendingRequestKey {
    hash: Hash,
    htl: u8,
}

impl PendingRequestKey {
    fn new(hash: Hash, htl: u8) -> Self {
        Self { hash, htl }
    }
}

struct PendingQuoteRequest {
    response_tx: oneshot::Sender<Option<NegotiatedQuote>>,
    preferred_mint_url: Option<String>,
    offered_payment_sat: u64,
}

struct PendingForwardRequest {
    requester_ids: HashSet<String>,
}

type PeerWireStats = PeerTrafficSnapshot;

struct PendingResponseSend {
    job_id: u64,
    peer_id: String,
    bytes: Vec<u8>,
    ready_at: Instant,
    queue_sequence: u64,
}

#[derive(Debug, Clone)]
struct NegotiatedQuote {
    peer_id: String,
    quote_id: u64,
    #[allow(dead_code)]
    mint_url: Option<String>,
}

struct IssuedQuote {
    expires_at: Instant,
    #[allow(dead_code)]
    payment_sat: u64,
    #[allow(dead_code)]
    mint_url: Option<String>,
}

#[derive(Debug, Clone)]
enum RouteFetchOutcome {
    Hit(Vec<u8>),
    Miss,
    Timeout,
}

const ACTIVE_PEER_REQUEST_RANK_PENALTY: usize = 3;

#[derive(Debug, Clone)]
struct MeshReadContext {
    exclude_peer_id: Option<String>,
    request_htl: u8,
    deadline: Option<Instant>,
    attempt_budget: Option<usize>,
}

impl Default for MeshReadContext {
    fn default() -> Self {
        Self {
            exclude_peer_id: None,
            request_htl: MAX_HTL,
            deadline: None,
            attempt_budget: None,
        }
    }
}

/// Aggregate stats from draining currently available peer-link messages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DataPumpStats {
    pub processed: usize,
    pub request_messages: usize,
    pub response_messages: usize,
    pub quote_request_messages: u64,
    pub quote_response_messages: u64,
    pub pubsub_interest_messages: u64,
    pub pubsub_frame_messages: u64,
    pub pubsub_inventory_messages: u64,
    pub pubsub_want_messages: u64,
    pub processed_bytes: u64,
}

/// Pubsub data delivered to a local subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PubsubEvent {
    pub stream_id: String,
    pub seq: u64,
    pub origin_peer_id: String,
    pub from_peer_id: String,
    pub payload: Vec<u8>,
}

/// Evidence that this peer won an outstanding, hash-verified block request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedBlockDelivery {
    pub hash: Hash,
    pub provider_peer_id: String,
    pub payload_bytes: u64,
}

/// One atomic drain of verified delivery evidence and any overflow since the prior drain.
///
/// Dropped evidence is intentionally not recoverable or billable through this API. An
/// application adapter must surface a non-zero count and must not infer a payment claim.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifiedBlockDeliveryBatch {
    pub deliveries: Vec<VerifiedBlockDelivery>,
    pub dropped_since_last_drain: u64,
}

#[derive(Default)]
struct VerifiedBlockDeliveryBuffer {
    deliveries: VecDeque<VerifiedBlockDelivery>,
    dropped_since_last_drain: u64,
}

/// Send-side accounting from a pubsub publish or forwarded pubsub message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PubsubPublishStats {
    pub selected_peers: usize,
    pub sent_peers: usize,
    pub sent_bytes: u64,
    pub deferred_peers: usize,
}

/// Production pubsub delivery strategy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PubsubDeliveryMode {
    /// Push full frames only along advertised interest routes.
    InterestPush,
    /// Route small inventories along advertised interest paths and pull payloads back along want paths.
    #[default]
    HtlInvWant,
}

/// Request dispatch strategy for peer queries.
///
/// Requests use bounded staged hedging by default. Callers may explicitly
/// raise the cap for controlled simulations, but production never needs an
/// unbounded sentinel.
#[derive(Debug, Clone, Copy)]
pub struct RequestDispatchConfig {
    /// Number of peers queried immediately.
    pub initial_fanout: usize,
    /// Number of additional peers to query on each hedge step.
    pub hedge_fanout: usize,
    /// Total peers allowed for this request.
    pub max_fanout: usize,
    /// Delay between hedge waves (ms). `0` means send all waves immediately.
    pub hedge_interval_ms: u64,
}

impl Default for RequestDispatchConfig {
    fn default() -> Self {
        Self {
            initial_fanout: 2,
            hedge_fanout: 1,
            max_fanout: 4,
            hedge_interval_ms: 50,
        }
    }
}

/// Normalize fanout config against current peer availability.
pub fn normalize_dispatch_config(
    dispatch: RequestDispatchConfig,
    available_peers: usize,
) -> RequestDispatchConfig {
    let mut cfg = dispatch;
    let cap = if cfg.max_fanout == 0 {
        available_peers
    } else {
        cfg.max_fanout.min(available_peers)
    };
    cfg.max_fanout = cap;
    cfg.initial_fanout = if cfg.initial_fanout == 0 {
        1
    } else {
        cfg.initial_fanout.min(cap.max(1))
    };
    cfg.hedge_fanout = if cfg.hedge_fanout == 0 {
        1
    } else {
        cfg.hedge_fanout.min(cap.max(1))
    };
    cfg
}

/// Build wave sizes for staged hedged dispatch.
pub fn build_hedged_wave_plan(peer_count: usize, dispatch: RequestDispatchConfig) -> Vec<usize> {
    if peer_count == 0 {
        return Vec::new();
    }
    let cap = dispatch.max_fanout.min(peer_count);
    if cap == 0 {
        return Vec::new();
    }

    let mut plan = Vec::new();
    let mut sent = 0usize;
    let first = dispatch.initial_fanout.min(cap).max(1);
    plan.push(first);
    sent += first;

    while sent < cap {
        let next = dispatch.hedge_fanout.min(cap - sent).max(1);
        plan.push(next);
        sent += next;
    }
    plan
}

/// Outcome returned after waiting on a hedged dispatch wave.
#[derive(Debug)]
pub enum HedgedWaveAction<T> {
    Continue,
    Success(T),
    Abort,
}

/// Run a staged hedged dispatch over peer index ranges.
///
/// This scheduler is shared by the reusable `MeshStoreCore` and the native
/// `hashtree-cli` mesh path so tests and production use the same wave timing.
pub async fn run_hedged_waves<T, SendWave, SendWaveFut, WaitWave, WaitWaveFut>(
    peer_count: usize,
    dispatch: RequestDispatchConfig,
    request_timeout: Duration,
    mut send_wave: SendWave,
    mut wait_wave: WaitWave,
) -> Option<T>
where
    SendWave: FnMut(Range<usize>) -> SendWaveFut,
    SendWaveFut: Future<Output = usize>,
    WaitWave: FnMut(Duration) -> WaitWaveFut,
    WaitWaveFut: Future<Output = HedgedWaveAction<T>>,
{
    let dispatch = normalize_dispatch_config(dispatch, peer_count);
    let wave_plan = build_hedged_wave_plan(peer_count, dispatch);
    if wave_plan.is_empty() {
        return None;
    }

    let deadline = Instant::now() + request_timeout;
    let mut sent_total = 0usize;
    let mut next_peer_idx = 0usize;

    for (wave_idx, wave_size) in wave_plan.iter().copied().enumerate() {
        let from = next_peer_idx;
        let to = (next_peer_idx + wave_size).min(peer_count);
        next_peer_idx = to;

        if from == to {
            continue;
        }

        sent_total += send_wave(from..to).await;
        if sent_total == 0 {
            if next_peer_idx >= peer_count {
                break;
            }
            continue;
        }

        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline.saturating_duration_since(now);
        let is_last_wave = wave_idx + 1 == wave_plan.len() || next_peer_idx >= peer_count;
        let wait = if is_last_wave {
            remaining
        } else if dispatch.hedge_interval_ms == 0 {
            Duration::ZERO
        } else {
            Duration::from_millis(dispatch.hedge_interval_ms).min(remaining)
        };

        if wait.is_zero() {
            continue;
        }

        match wait_wave(wait).await {
            HedgedWaveAction::Continue => {}
            HedgedWaveAction::Success(value) => return Some(value),
            HedgedWaveAction::Abort => break,
        }
    }

    None
}

/// Keep selector membership aligned with currently connected peer IDs.
pub async fn sync_selector_peers(selector: &RwLock<PeerSelector>, current_peer_ids: &[String]) {
    let mut selector = selector.write().await;
    let current: HashSet<&str> = current_peer_ids.iter().map(String::as_str).collect();
    let known: Vec<String> = selector.all_stats().map(|s| s.peer_id.clone()).collect();
    for peer_id in known {
        if !current.contains(peer_id.as_str()) {
            selector.remove_peer(&peer_id);
        }
    }
    for peer_id in current_peer_ids {
        selector.add_peer(peer_id.clone());
    }
}

/// Response behavior profile for simulation/game-theory actors.
///
/// Defaults to honest behavior (always respond correctly, no extra delay).
#[derive(Debug, Clone, Copy)]
pub struct ResponseBehaviorConfig {
    /// Probability that a node drops a response even when it has data.
    pub drop_response_prob: f64,
    /// Probability that a node responds with corrupted payload.
    pub corrupt_response_prob: f64,
    /// Baseline response delay before a peer starts sending any data.
    pub extra_delay_ms: u64,
    /// Additional delay before the first response byte becomes available.
    pub first_byte_delay_ms: u64,
    /// Sustained throughput for delivering large payloads. `0` disables size-based slowdown.
    pub bytes_per_second: u64,
    /// Probability that an otherwise honest response experiences an extra stall.
    pub stall_response_prob: f64,
    /// Extra delay injected when a stall event happens.
    pub stall_delay_ms: u64,
}

impl Default for ResponseBehaviorConfig {
    fn default() -> Self {
        Self {
            drop_response_prob: 0.0,
            corrupt_response_prob: 0.0,
            extra_delay_ms: 0,
            first_byte_delay_ms: 0,
            bytes_per_second: 0,
            stall_response_prob: 0.0,
            stall_delay_ms: 0,
        }
    }
}

impl ResponseBehaviorConfig {
    fn normalized(self) -> Self {
        Self {
            drop_response_prob: self.drop_response_prob.clamp(0.0, 1.0),
            corrupt_response_prob: self.corrupt_response_prob.clamp(0.0, 1.0),
            extra_delay_ms: self.extra_delay_ms,
            first_byte_delay_ms: self.first_byte_delay_ms,
            bytes_per_second: self.bytes_per_second,
            stall_response_prob: self.stall_response_prob.clamp(0.0, 1.0),
            stall_delay_ms: self.stall_delay_ms,
        }
    }
}

/// Routing policy for request ordering + dispatch fanout.
#[derive(Debug, Clone)]
pub struct MeshRoutingConfig {
    pub selection_strategy: SelectionStrategy,
    pub fairness_enabled: bool,
    /// Blend weight for payment-priority ranking in selector (`0.0` disables).
    pub cashu_payment_weight: f64,
    /// Refuse serving peers that have reached this many unpaid post-delivery settlements.
    /// `0` disables refusal and only keeps metadata/downranking.
    pub cashu_payment_default_block_threshold: u64,
    /// Cashu mint URLs this node is willing to use for settlement.
    pub cashu_accepted_mints: Vec<String>,
    /// Preferred Cashu mint URL when initiating paid retrieval.
    pub cashu_default_mint: Option<String>,
    /// Baseline cap for accepting a peer-suggested mint outside the trusted set.
    pub cashu_peer_suggested_mint_base_cap_sat: u64,
    /// Additional sats allowed per successful delivery from that peer.
    pub cashu_peer_suggested_mint_success_step_sat: u64,
    /// Additional sats allowed per successful post-delivery payment received from that peer.
    pub cashu_peer_suggested_mint_receipt_step_sat: u64,
    /// Hard upper bound for any single peer-suggested mint quote we accept.
    pub cashu_peer_suggested_mint_max_cap_sat: u64,
    pub dispatch: RequestDispatchConfig,
    pub response_behavior: ResponseBehaviorConfig,
    pub pubsub_scheduler: PubsubSchedulerConfig,
    pub pubsub_delivery_mode: PubsubDeliveryMode,
    /// Forward peer pubsub interests, inventories, and payloads for downstream peers.
    pub pubsub_forwarding: bool,
    /// Initial hops-to-live for locally originated pubsub interest/inventory frames.
    pub pubsub_max_htl: u8,
}

impl Default for MeshRoutingConfig {
    fn default() -> Self {
        Self {
            selection_strategy: SelectionStrategy::Weighted,
            fairness_enabled: true,
            cashu_payment_weight: 0.0,
            cashu_payment_default_block_threshold: 0,
            cashu_accepted_mints: Vec::new(),
            cashu_default_mint: None,
            cashu_peer_suggested_mint_base_cap_sat: 0,
            cashu_peer_suggested_mint_success_step_sat: 0,
            cashu_peer_suggested_mint_receipt_step_sat: 0,
            cashu_peer_suggested_mint_max_cap_sat: 0,
            dispatch: RequestDispatchConfig::default(),
            response_behavior: ResponseBehaviorConfig::default(),
            pubsub_scheduler: PubsubSchedulerConfig::default(),
            pubsub_delivery_mode: PubsubDeliveryMode::HtlInvWant,
            pubsub_forwarding: true,
            pubsub_max_htl: MESH_EVENT_POLICY.max_htl,
        }
    }
}

impl MeshRoutingConfig {
    fn pubsub_initial_htl(&self) -> u8 {
        self.pubsub_max_htl.clamp(1, MAX_HTL)
    }
}

/// Routed mesh store core that works with any storage backend and transport
/// implementation.
///
/// This is the shared code between production and simulation.
/// - Production: transport-specific crates compose `MeshStoreCore` with their links
/// - Simulation: `MeshStoreCore<MemoryStore, MockRelayTransport, MockConnectionFactory>`
pub struct MeshStoreCore<S, R, F>
where
    S: Store + Send + Sync + 'static,
    R: SignalingTransport + Send + Sync + 'static,
    F: PeerLinkFactory + Send + Sync + 'static,
{
    /// Local backing store
    local_store: Arc<S>,
    /// Mesh router (handles peer discovery and connection)
    signaling: Arc<MeshRouter<R, F>>,
    /// Per-peer HTL config
    htl_configs: RwLock<HashMap<String, PeerHTLConfig>>,
    /// Pending requests we sent
    pending_requests: RwLock<HashMap<PendingRequestKey, Vec<PendingRequest>>>,
    /// Pending quote negotiations keyed by requested hash.
    pending_quotes: RwLock<HashMap<String, PendingQuoteRequest>>,
    /// Forwarded peer requests currently being resolved through the mesh/upstream.
    pending_forward_requests: RwLock<HashMap<PendingRequestKey, PendingForwardRequest>>,
    /// Bounded negative cache for recently forwarded misses/timeouts.
    /// Quotes we issued to peers and will accept exactly once until expiry.
    issued_quotes: RwLock<HashMap<(String, String, u64), IssuedQuote>>,
    /// Monotonic quote identifier generator.
    next_quote_id: RwLock<u64>,
    /// Adaptive selector for peer ordering.
    peer_selector: RwLock<PeerSelector>,
    /// Active per-peer in-flight reads so concurrent block fetches spread across peers.
    peer_active_requests: RwLock<HashMap<String, usize>>,
    /// Actual wire traffic stats used for upload-side reciprocity scheduling.
    peer_wire_stats: RwLock<HashMap<String, PeerWireStats>>,
    /// Streams this node wants delivered locally.
    pubsub_local_interests: RwLock<HashSet<String>>,
    /// Current sequence per local stream interest.
    pubsub_local_interest_versions: RwLock<HashMap<String, u64>>,
    /// Reverse pubsub routes: stream id -> peers with local/downstream interest.
    pubsub_peer_interests: RwLock<HashMap<String, HashSet<String>>>,
    /// Route owner for each downstream subscriber interest.
    pubsub_interest_routes: RwLock<HashMap<(String, String), String>>,
    /// Latest interest sequence observed per subscriber/stream.
    pubsub_interest_versions: RwLock<HashMap<(String, String), u64>>,
    /// Bounded dedupe for pubsub interest floods.
    pubsub_seen_interests: Mutex<TimedSeenSet>,
    /// Bounded dedupe for pubsub data frames.
    pubsub_seen_frames: Mutex<TimedSeenSet>,
    /// Bounded dedupe for pubsub inventory floods.
    pubsub_seen_inventories: Mutex<TimedSeenSet>,
    /// Bounded dedupe for pubsub wants by requesting peer.
    pubsub_seen_wants: Mutex<TimedSeenSet>,
    /// First upstream peer that announced each inventory key.
    pubsub_inventory_routes: RwLock<HashMap<String, String>>,
    /// Downstream peers waiting for a payload after sending a want.
    pubsub_want_routes: RwLock<HashMap<String, HashSet<String>>>,
    /// Dedupe for wants this node already sent upstream.
    pubsub_upstream_wants: Mutex<TimedSeenSet>,
    /// Small payload cache for serving wants after inventory-first announcements.
    pubsub_frame_cache: Mutex<VecDeque<(String, PubsubFrame)>>,
    /// Local pubsub delivery inbox.
    pubsub_inbox: Mutex<VecDeque<PubsubEvent>>,
    /// Bounded application-facing evidence for first-winner block deliveries.
    verified_block_deliveries: Mutex<VerifiedBlockDeliveryBuffer>,
    /// Wakes consumers waiting for local pubsub deliveries.
    pubsub_notify: Notify,
    /// Per stream/peer deferred counts for aging pubsub strategies.
    pubsub_deferred_counts: RwLock<HashMap<(String, String), u64>>,
    /// Monotonic sequence for locally originated pubsub interest updates.
    next_pubsub_interest_seq: AtomicU64,
    /// Pending content responses waiting for upload arbitration.
    pending_response_sends: Mutex<Vec<PendingResponseSend>>,
    /// Upload response scheduler state.
    response_scheduler_running: AtomicBool,
    /// Monotonic id for queued response sends.
    next_response_job_id: AtomicU64,
    /// Routing/dispatch configuration.
    routing: MeshRoutingConfig,
    /// Request timeout
    request_timeout: Duration,
    /// Debug mode
    debug: bool,
    /// Running flag
    running: RwLock<bool>,
}

impl<S, R, F> MeshStoreCore<S, R, F>
where
    S: Store + Send + Sync + 'static,
    R: SignalingTransport + Send + Sync + 'static,
    F: PeerLinkFactory + Send + Sync + 'static,
{
    /// Create a new routed mesh store core.
    pub fn new(
        local_store: Arc<S>,
        signaling: Arc<MeshRouter<R, F>>,
        request_timeout: Duration,
        debug: bool,
    ) -> Self {
        Self::new_with_routing(
            local_store,
            signaling,
            request_timeout,
            debug,
            Default::default(),
        )
    }

    /// Create a new routed mesh store core with explicit routing configuration.
    pub fn new_with_routing(
        local_store: Arc<S>,
        signaling: Arc<MeshRouter<R, F>>,
        request_timeout: Duration,
        debug: bool,
        routing: MeshRoutingConfig,
    ) -> Self {
        let mut selector = PeerSelector::with_strategy(routing.selection_strategy);
        selector.set_fairness(routing.fairness_enabled);
        selector.set_cashu_payment_weight(routing.cashu_payment_weight);
        Self {
            local_store,
            signaling,
            htl_configs: RwLock::new(HashMap::new()),
            pending_requests: RwLock::new(HashMap::new()),
            pending_quotes: RwLock::new(HashMap::new()),
            pending_forward_requests: RwLock::new(HashMap::new()),
            issued_quotes: RwLock::new(HashMap::new()),
            next_quote_id: RwLock::new(1),
            peer_selector: RwLock::new(selector),
            peer_active_requests: RwLock::new(HashMap::new()),
            peer_wire_stats: RwLock::new(HashMap::new()),
            pubsub_local_interests: RwLock::new(HashSet::new()),
            pubsub_local_interest_versions: RwLock::new(HashMap::new()),
            pubsub_peer_interests: RwLock::new(HashMap::new()),
            pubsub_interest_routes: RwLock::new(HashMap::new()),
            pubsub_interest_versions: RwLock::new(HashMap::new()),
            pubsub_seen_interests: Mutex::new(TimedSeenSet::new(
                PUBSUB_SEEN_CAPACITY,
                PUBSUB_SEEN_TTL,
            )),
            pubsub_seen_frames: Mutex::new(TimedSeenSet::new(
                PUBSUB_SEEN_CAPACITY,
                PUBSUB_SEEN_TTL,
            )),
            pubsub_seen_inventories: Mutex::new(TimedSeenSet::new(
                PUBSUB_SEEN_CAPACITY,
                PUBSUB_SEEN_TTL,
            )),
            pubsub_seen_wants: Mutex::new(TimedSeenSet::new(PUBSUB_SEEN_CAPACITY, PUBSUB_SEEN_TTL)),
            pubsub_inventory_routes: RwLock::new(HashMap::new()),
            pubsub_want_routes: RwLock::new(HashMap::new()),
            pubsub_upstream_wants: Mutex::new(TimedSeenSet::new(
                PUBSUB_SEEN_CAPACITY,
                PUBSUB_SEEN_TTL,
            )),
            pubsub_frame_cache: Mutex::new(VecDeque::new()),
            pubsub_inbox: Mutex::new(VecDeque::new()),
            verified_block_deliveries: Mutex::new(VerifiedBlockDeliveryBuffer::default()),
            pubsub_notify: Notify::new(),
            pubsub_deferred_counts: RwLock::new(HashMap::new()),
            next_pubsub_interest_seq: AtomicU64::new(1),
            pending_response_sends: Mutex::new(Vec::new()),
            response_scheduler_running: AtomicBool::new(false),
            next_response_job_id: AtomicU64::new(1),
            routing,
            request_timeout,
            debug,
            running: RwLock::new(false),
        }
    }

    /// Start the store (begin listening for messages)
    pub async fn start(&self) -> Result<(), TransportError> {
        *self.running.write().await = true;

        // Send initial hello
        self.signaling.send_hello(vec![]).await?;

        Ok(())
    }

    /// Stop the store
    pub async fn stop(&self) {
        *self.running.write().await = false;
    }

    /// Process incoming signaling message
    pub async fn process_signaling(&self, msg: SignalingMessage) -> Result<(), TransportError> {
        // When a new peer connects, initialize their HTL config
        let peer_id = msg.peer_id().to_string();
        {
            let mut configs = self.htl_configs.write().await;
            if !configs.contains_key(&peer_id) {
                configs.insert(peer_id.clone(), PeerHTLConfig::random());
            }
        }
        self.peer_selector.write().await.add_peer(peer_id.clone());

        let result = self.signaling.handle_message(msg).await;
        if result.is_ok() {
            self.announce_pubsub_interests_to_peer(&peer_id).await;
        }
        result
    }

    /// Get signaling manager reference
    pub fn signaling(&self) -> &Arc<MeshRouter<R, F>> {
        &self.signaling
    }

    fn response_behavior(&self) -> ResponseBehaviorConfig {
        self.routing.response_behavior.normalized()
    }

    async fn record_peer_wire_sent(&self, peer_id: &str, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let mut stats = self.peer_wire_stats.write().await;
        let entry = stats.entry(peer_id.to_string()).or_default();
        entry.bytes_sent = entry.bytes_sent.saturating_add(bytes);
    }

    async fn record_peer_wire_received(&self, peer_id: &str, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let mut stats = self.peer_wire_stats.write().await;
        let entry = stats.entry(peer_id.to_string()).or_default();
        entry.bytes_received = entry.bytes_received.saturating_add(bytes);
    }

    /// Record ingress from a peer that matched local or downstream demand.
    ///
    /// Raw bytes are tracked separately in `record_peer_wire_received`; this
    /// counter is the reciprocity signal used by shared outbound scheduling.
    pub async fn record_useful_bytes_received_from_peer(&self, peer_id: &str, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let mut stats = self.peer_wire_stats.write().await;
        let entry = stats.entry(peer_id.to_string()).or_default();
        entry.useful_bytes_received = entry.useful_bytes_received.saturating_add(bytes);
    }

    /// Snapshot peer traffic for production pubsub scheduling or diagnostics.
    pub async fn peer_traffic_snapshot(&self, peer_id: &str) -> PeerTrafficSnapshot {
        self.peer_wire_stats
            .read()
            .await
            .get(peer_id)
            .copied()
            .unwrap_or_default()
    }

    /// Snapshot all known peer traffic for production pubsub scheduling.
    pub async fn peer_traffic_snapshots(&self) -> HashMap<String, PeerTrafficSnapshot> {
        self.peer_wire_stats.read().await.clone()
    }

    fn pubsub_key(origin_peer_id: &str, stream_id: &str, seq: u64) -> String {
        format!("{origin_peer_id}:{stream_id}:{seq}")
    }

    fn pubsub_frame_key(frame: &PubsubFrame) -> String {
        Self::pubsub_key(&frame.origin_peer_id, &frame.stream_id, frame.seq)
    }

    fn pubsub_interest_key(interest: &PubsubInterest) -> String {
        format!(
            "{}:{}:{}:{}",
            interest.subscriber_peer_id, interest.stream_id, interest.seq, interest.active
        )
    }

    fn next_pubsub_interest_seq(&self) -> u64 {
        self.next_pubsub_interest_seq
            .fetch_add(1, Ordering::Relaxed)
    }

    async fn record_peer_pubsub_wire_sent(&self, peer_id: &str, bytes: u64, bandwidth_debt: f64) {
        if bytes == 0 {
            return;
        }
        let mut stats = self.peer_wire_stats.write().await;
        let entry = stats.entry(peer_id.to_string()).or_default();
        entry.bytes_sent = entry.bytes_sent.saturating_add(bytes);
        entry.bandwidth_debt = bandwidth_debt;
    }

    async fn send_pubsub_interest_to_peers(
        &self,
        interest: &PubsubInterest,
        exclude_peer_id: Option<&str>,
    ) -> PubsubPublishStats {
        if !should_forward_htl(interest.htl) {
            return PubsubPublishStats::default();
        }

        let mut peer_ids = self.signaling.peer_ids().await;
        peer_ids.sort();
        peer_ids.retain(|peer_id| exclude_peer_id.is_none_or(|exclude| peer_id != exclude));

        let bytes = encode_pubsub_interest(interest);
        let mut stats = PubsubPublishStats {
            selected_peers: peer_ids.len(),
            ..Default::default()
        };
        for peer_id in peer_ids {
            let Some(channel) = self.signaling.get_channel(&peer_id).await else {
                continue;
            };
            if channel.send(bytes.clone()).await.is_ok() {
                stats.sent_peers += 1;
                stats.sent_bytes = stats.sent_bytes.saturating_add(bytes.len() as u64);
                self.record_peer_wire_sent(&peer_id, bytes.len() as u64)
                    .await;
            }
        }
        stats
    }

    async fn announce_pubsub_interests_to_peer(&self, peer_id: &str) {
        let mut interests = self
            .pubsub_local_interests
            .read()
            .await
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        interests.sort();
        if interests.is_empty() {
            return;
        }

        let interests = {
            let versions = self.pubsub_local_interest_versions.read().await;
            interests
                .into_iter()
                .filter_map(|stream_id| {
                    versions
                        .get(&stream_id)
                        .copied()
                        .map(|seq| (stream_id, seq))
                })
                .collect::<Vec<_>>()
        };

        for (stream_id, seq) in interests {
            let interest = create_pubsub_interest(
                stream_id,
                self.signaling.peer_id().to_string(),
                seq,
                true,
                MAX_HTL,
            );
            let Some(channel) = self.signaling.get_channel(peer_id).await else {
                continue;
            };
            let bytes = encode_pubsub_interest(&interest);
            if channel.send(bytes.clone()).await.is_ok() {
                self.record_peer_wire_sent(peer_id, bytes.len() as u64)
                    .await;
            }
        }
    }

    fn remove_pubsub_peer_interest(
        peer_interests: &mut HashMap<String, HashSet<String>>,
        routes: &HashMap<(String, String), String>,
        stream_id: &str,
        peer_id: &str,
    ) {
        let still_has_route = routes
            .iter()
            .any(|((stream, _subscriber), peer)| stream == stream_id && peer == peer_id);
        if still_has_route {
            return;
        }
        if let Some(peers) = peer_interests.get_mut(stream_id) {
            peers.remove(peer_id);
            if peers.is_empty() {
                peer_interests.remove(stream_id);
            }
        }
    }

    async fn apply_pubsub_interest_route(
        &self,
        from_peer: &str,
        interest: &PubsubInterest,
    ) -> bool {
        if interest.stream_id.is_empty() || interest.subscriber_peer_id.is_empty() {
            return false;
        }
        if interest.subscriber_peer_id == self.signaling.peer_id() {
            return false;
        }

        let interest_key = Self::pubsub_interest_key(interest);
        if !self
            .pubsub_seen_interests
            .lock()
            .await
            .insert_if_new(interest_key)
        {
            return false;
        }

        let route_key = (
            interest.stream_id.clone(),
            interest.subscriber_peer_id.clone(),
        );
        {
            let mut versions = self.pubsub_interest_versions.write().await;
            if versions
                .get(&route_key)
                .is_some_and(|latest| *latest >= interest.seq)
            {
                return false;
            }
            versions.insert(route_key.clone(), interest.seq);
        }

        let mut peer_interests = self.pubsub_peer_interests.write().await;
        let mut routes = self.pubsub_interest_routes.write().await;
        if interest.active {
            if let Some(previous_peer) = routes.insert(route_key, from_peer.to_string()) {
                if previous_peer != from_peer {
                    Self::remove_pubsub_peer_interest(
                        &mut peer_interests,
                        &routes,
                        &interest.stream_id,
                        &previous_peer,
                    );
                }
            }
            peer_interests
                .entry(interest.stream_id.clone())
                .or_default()
                .insert(from_peer.to_string());
        } else if let Some(previous_peer) = routes.remove(&route_key) {
            Self::remove_pubsub_peer_interest(
                &mut peer_interests,
                &routes,
                &interest.stream_id,
                &previous_peer,
            );
        } else {
            Self::remove_pubsub_peer_interest(
                &mut peer_interests,
                &routes,
                &interest.stream_id,
                from_peer,
            );
        }

        true
    }

    async fn interested_pubsub_peers(
        &self,
        stream_id: &str,
        exclude_peer_id: Option<&str>,
    ) -> Vec<String> {
        let connected = self
            .signaling
            .peer_ids()
            .await
            .into_iter()
            .collect::<HashSet<_>>();
        let mut peers = self
            .pubsub_peer_interests
            .read()
            .await
            .get(stream_id)
            .map(|peers| peers.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        peers.retain(|peer_id| {
            connected.contains(peer_id) && exclude_peer_id.is_none_or(|exclude| peer_id != exclude)
        });
        peers.sort();
        peers
    }

    async fn decrement_pubsub_htl_for_peer(&self, peer_id: &str, htl: u8) -> u8 {
        let htl_config = {
            let configs = self.htl_configs.read().await;
            configs
                .get(peer_id)
                .cloned()
                .unwrap_or_else(PeerHTLConfig::random)
        };
        htl_config.decrement_with_policy(htl, &MESH_EVENT_POLICY)
    }

    async fn send_pubsub_inventory_to_peers(
        &self,
        inv: &PubsubInventory,
        peer_ids: &[String],
    ) -> PubsubPublishStats {
        if peer_ids.is_empty() || !should_forward_htl(inv.htl) {
            return PubsubPublishStats::default();
        }

        let mut stats = PubsubPublishStats {
            selected_peers: peer_ids.len(),
            ..Default::default()
        };
        for peer_id in peer_ids {
            let send_htl = self.decrement_pubsub_htl_for_peer(peer_id, inv.htl).await;
            if !should_forward_htl(send_htl) {
                continue;
            }
            let Some(channel) = self.signaling.get_channel(peer_id).await else {
                continue;
            };
            let mut outgoing = inv.clone();
            outgoing.htl = send_htl;
            let bytes = encode_pubsub_inventory(&outgoing);
            let message_bytes = bytes.len() as u64;
            if channel.send(bytes).await.is_ok() {
                stats.sent_peers += 1;
                stats.sent_bytes = stats.sent_bytes.saturating_add(message_bytes);
                self.record_peer_wire_sent(peer_id, message_bytes).await;
            }
        }
        stats
    }

    async fn send_pubsub_want_to_peer(&self, want: &PubsubWant, peer_id: &str) -> bool {
        let Some(channel) = self.signaling.get_channel(peer_id).await else {
            return false;
        };
        let bytes = encode_pubsub_want(want);
        let message_bytes = bytes.len() as u64;
        match channel.send(bytes).await {
            Ok(()) => {
                self.record_peer_wire_sent(peer_id, message_bytes).await;
                true
            }
            Err(_) => false,
        }
    }

    async fn send_pubsub_want_upstream(
        &self,
        key: &str,
        want: &PubsubWant,
        exclude_peer_id: Option<&str>,
    ) -> bool {
        let upstream = {
            let routes = self.pubsub_inventory_routes.read().await;
            routes.get(key).cloned()
        };
        let Some(upstream) = upstream else {
            return false;
        };
        if exclude_peer_id.is_some_and(|exclude| exclude == upstream) {
            return false;
        }
        let want_key = format!("{key}:{upstream}");
        if !self
            .pubsub_upstream_wants
            .lock()
            .await
            .insert_if_new(want_key)
        {
            return false;
        }
        self.send_pubsub_want_to_peer(want, &upstream).await
    }

    async fn cache_pubsub_frame(&self, key: String, frame: PubsubFrame) {
        let mut cache = self.pubsub_frame_cache.lock().await;
        if let Some(index) = cache.iter().position(|(cached_key, _)| cached_key == &key) {
            cache.remove(index);
        }
        cache.push_back((key, frame));
        while cache.len() > PUBSUB_FRAME_CACHE_CAPACITY {
            cache.pop_front();
        }
    }

    async fn cached_pubsub_frame(&self, key: &str) -> Option<PubsubFrame> {
        self.pubsub_frame_cache
            .lock()
            .await
            .iter()
            .find_map(|(cached_key, frame)| {
                if cached_key == key {
                    Some(frame.clone())
                } else {
                    None
                }
            })
    }

    async fn remember_pubsub_want_peer(&self, key: String, from_peer: &str) -> bool {
        let mut routes = self.pubsub_want_routes.write().await;
        routes.entry(key).or_default().insert(from_peer.to_string())
    }

    async fn take_pubsub_want_peers(
        &self,
        key: &str,
        exclude_peer_id: Option<&str>,
    ) -> Vec<String> {
        let connected = self
            .signaling
            .peer_ids()
            .await
            .into_iter()
            .collect::<HashSet<_>>();
        let mut peers = self
            .pubsub_want_routes
            .write()
            .await
            .remove(key)
            .map(|peers| peers.into_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        peers.retain(|peer_id| {
            connected.contains(peer_id) && exclude_peer_id.is_none_or(|exclude| peer_id != exclude)
        });
        peers.sort();
        peers
    }

    async fn select_pubsub_peers(
        &self,
        stream_id: &str,
        seq: u64,
        message_bytes: u64,
        peer_ids: &[String],
    ) -> (Vec<String>, Vec<String>) {
        let traffic = self.peer_wire_stats.read().await;
        let deferred_counts = self.pubsub_deferred_counts.read().await;
        let candidates = peer_ids
            .iter()
            .map(|peer_id| PubsubCandidate {
                peer_id: peer_id.clone(),
                traffic: traffic.get(peer_id).copied().unwrap_or_default(),
                deferred_count: deferred_counts
                    .get(&(stream_id.to_string(), peer_id.clone()))
                    .copied()
                    .unwrap_or_default(),
            })
            .collect::<Vec<_>>();
        drop(deferred_counts);
        drop(traffic);

        let selection = self.routing.pubsub_scheduler.select(
            stream_id,
            seq,
            self.signaling.peer_id(),
            message_bytes,
            &candidates,
        );

        {
            let mut deferred_counts = self.pubsub_deferred_counts.write().await;
            for peer_id in &selection.deferred {
                *deferred_counts
                    .entry((stream_id.to_string(), peer_id.clone()))
                    .or_insert(0) += 1;
            }
            for peer_id in &selection.selected {
                deferred_counts.remove(&(stream_id.to_string(), peer_id.clone()));
            }
        }

        (selection.selected, selection.deferred)
    }

    async fn send_pubsub_frame_to_peers(
        &self,
        frame: &PubsubFrame,
        peer_ids: &[String],
    ) -> PubsubPublishStats {
        if peer_ids.is_empty() || !should_forward_htl(frame.htl) {
            return PubsubPublishStats::default();
        }

        let bytes = encode_pubsub_frame(frame);
        let message_bytes = bytes.len() as u64;
        let (selected, deferred) = self
            .select_pubsub_peers(&frame.stream_id, frame.seq, message_bytes, peer_ids)
            .await;
        let mut stats = PubsubPublishStats {
            selected_peers: selected.len(),
            deferred_peers: deferred.len(),
            ..Default::default()
        };

        for peer_id in selected {
            let Some(channel) = self.signaling.get_channel(&peer_id).await else {
                continue;
            };
            let snapshot = self.peer_traffic_snapshot(&peer_id).await;
            let bandwidth_debt = reciprocal_virtual_finish(snapshot, message_bytes);
            if channel.send(bytes.clone()).await.is_ok() {
                stats.sent_peers += 1;
                stats.sent_bytes = stats.sent_bytes.saturating_add(message_bytes);
                self.record_peer_pubsub_wire_sent(&peer_id, message_bytes, bandwidth_debt)
                    .await;
            }
        }

        stats
    }

    async fn enqueue_pubsub_event(&self, event: PubsubEvent) {
        let mut inbox = self.pubsub_inbox.lock().await;
        inbox.push_back(event);
        while inbox.len() > PUBSUB_INBOX_CAPACITY {
            inbox.pop_front();
        }
        self.pubsub_notify.notify_one();
    }

    /// Subscribe this node to a pubsub stream and advertise that interest.
    pub async fn subscribe_pubsub(
        self: &Arc<Self>,
        stream_id: impl Into<String>,
    ) -> PubsubPublishStats {
        let stream_id = stream_id.into();
        if stream_id.is_empty() {
            return PubsubPublishStats::default();
        }
        self.pubsub_local_interests
            .write()
            .await
            .insert(stream_id.clone());
        let seq = {
            let mut versions = self.pubsub_local_interest_versions.write().await;
            match versions.get(&stream_id).copied() {
                Some(seq) => seq,
                None => {
                    let seq = self.next_pubsub_interest_seq();
                    versions.insert(stream_id.clone(), seq);
                    seq
                }
            }
        };
        let interest = create_pubsub_interest(
            stream_id,
            self.signaling.peer_id().to_string(),
            seq,
            true,
            self.routing.pubsub_initial_htl(),
        );
        self.send_pubsub_interest_to_peers(&interest, None).await
    }

    /// Stop local delivery for a pubsub stream and advertise the withdrawn interest.
    pub async fn unsubscribe_pubsub(
        self: &Arc<Self>,
        stream_id: impl Into<String>,
    ) -> PubsubPublishStats {
        let stream_id = stream_id.into();
        if stream_id.is_empty() {
            return PubsubPublishStats::default();
        }
        self.pubsub_local_interests.write().await.remove(&stream_id);
        self.pubsub_local_interest_versions
            .write()
            .await
            .remove(&stream_id);
        let interest = create_pubsub_interest(
            stream_id,
            self.signaling.peer_id().to_string(),
            self.next_pubsub_interest_seq(),
            false,
            self.routing.pubsub_initial_htl(),
        );
        self.send_pubsub_interest_to_peers(&interest, None).await
    }

    /// Publish bytes on a pubsub stream through the configured mesh delivery mode.
    pub async fn publish_pubsub(
        self: &Arc<Self>,
        stream_id: impl Into<String>,
        seq: u64,
        payload: Vec<u8>,
    ) -> PubsubPublishStats {
        let stream_id = stream_id.into();
        if stream_id.is_empty() {
            return PubsubPublishStats::default();
        }
        let payload_bytes = payload.len() as u64;
        let frame = create_pubsub_frame(
            stream_id.clone(),
            seq,
            self.signaling.peer_id().to_string(),
            payload.clone(),
            self.routing.pubsub_initial_htl(),
        );
        let frame_key = Self::pubsub_frame_key(&frame);
        self.pubsub_seen_frames
            .lock()
            .await
            .insert_if_new(frame_key.clone());
        self.cache_pubsub_frame(frame_key, frame.clone()).await;

        if self
            .pubsub_local_interests
            .read()
            .await
            .contains(&stream_id)
        {
            self.enqueue_pubsub_event(PubsubEvent {
                stream_id: stream_id.clone(),
                seq,
                origin_peer_id: self.signaling.peer_id().to_string(),
                from_peer_id: self.signaling.peer_id().to_string(),
                payload,
            })
            .await;
        }

        match self.routing.pubsub_delivery_mode {
            PubsubDeliveryMode::InterestPush => {
                let peers = self.interested_pubsub_peers(&stream_id, None).await;
                self.send_pubsub_frame_to_peers(&frame, &peers).await
            }
            PubsubDeliveryMode::HtlInvWant => {
                let inv = create_pubsub_inventory(
                    stream_id,
                    seq,
                    self.signaling.peer_id().to_string(),
                    payload_bytes,
                    self.routing.pubsub_initial_htl(),
                );
                let peers = self.interested_pubsub_peers(&inv.stream_id, None).await;
                self.send_pubsub_inventory_to_peers(&inv, &peers).await
            }
        }
    }

    /// Drain locally delivered pubsub events.
    pub async fn drain_pubsub_events(&self) -> Vec<PubsubEvent> {
        self.pubsub_inbox.lock().await.drain(..).collect()
    }

    /// Drain verified first-winner block deliveries for an application adapter.
    pub async fn drain_verified_block_deliveries(&self) -> VerifiedBlockDeliveryBatch {
        let mut buffer = self.verified_block_deliveries.lock().await;
        VerifiedBlockDeliveryBatch {
            deliveries: buffer.deliveries.drain(..).collect(),
            dropped_since_last_drain: std::mem::take(&mut buffer.dropped_since_last_drain),
        }
    }

    /// Wait until a locally delivered pubsub event is available, then return it.
    pub async fn recv_pubsub_event(&self) -> PubsubEvent {
        loop {
            if let Some(event) = self.pubsub_inbox.lock().await.pop_front() {
                return event;
            }
            self.pubsub_notify.notified().await;
        }
    }

    /// Connected peers that currently have local or downstream interest in a stream.
    pub async fn pubsub_interest_peers(&self, stream_id: &str) -> Vec<String> {
        self.interested_pubsub_peers(stream_id, None).await
    }

    fn choose_ready_response_job(
        ready_jobs: &[(u64, String, usize, Instant, u64)],
        stats: &HashMap<String, PeerWireStats>,
    ) -> Option<(u64, f64)> {
        let jobs = ready_jobs
            .iter()
            .map(|job| OutboundJobCandidate {
                job_id: job.0,
                peer_id: job.1.clone(),
                message_bytes: job.2 as u64,
                queue_sequence: job.4,
            })
            .collect::<Vec<_>>();
        select_reciprocal_outbound_job(&jobs, |peer_id| {
            stats.get(peer_id).copied().unwrap_or_default()
        })
        .map(|choice| (choice.job_id, choice.virtual_finish))
    }

    async fn enqueue_response_send(
        self: &Arc<Self>,
        peer_id: String,
        bytes: Vec<u8>,
        ready_at: Instant,
    ) {
        let job_id = self.next_response_job_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut queue = self.pending_response_sends.lock().await;
            queue.push(PendingResponseSend {
                job_id,
                peer_id,
                bytes,
                ready_at,
                queue_sequence: job_id,
            });
        }

        if self
            .response_scheduler_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let this = Arc::clone(self);
            tokio::spawn(async move {
                this.run_response_scheduler().await;
            });
        }
    }

    async fn run_response_scheduler(self: Arc<Self>) {
        loop {
            let snapshot = {
                let queue = self.pending_response_sends.lock().await;
                if queue.is_empty() {
                    self.response_scheduler_running
                        .store(false, Ordering::Release);
                    return;
                }
                queue
                    .iter()
                    .map(|job| {
                        (
                            job.job_id,
                            job.peer_id.clone(),
                            job.bytes.len(),
                            job.ready_at,
                            job.queue_sequence,
                        )
                    })
                    .collect::<Vec<_>>()
            };

            let now = Instant::now();
            let mut earliest_ready_at: Option<Instant> = None;
            let mut ready_jobs = Vec::new();
            for job in &snapshot {
                if job.3 <= now {
                    ready_jobs.push(job.clone());
                } else {
                    earliest_ready_at = Some(match earliest_ready_at {
                        Some(current) => current.min(job.3),
                        None => job.3,
                    });
                }
            }

            if ready_jobs.is_empty() {
                if let Some(ready_at) = earliest_ready_at {
                    tokio::time::sleep(ready_at.saturating_duration_since(Instant::now())).await;
                    continue;
                }
                self.response_scheduler_running
                    .store(false, Ordering::Release);
                return;
            }

            let (selected_job_id, selected_finish) = {
                let stats = self.peer_wire_stats.read().await;
                Self::choose_ready_response_job(&ready_jobs, &stats).expect("ready response job")
            };

            let selected = {
                let mut queue = self.pending_response_sends.lock().await;
                let Some(index) = queue.iter().position(|job| job.job_id == selected_job_id) else {
                    continue;
                };
                queue.swap_remove(index)
            };

            let sent = if let Some(channel) = self.signaling.get_channel(&selected.peer_id).await {
                channel.send(selected.bytes.clone()).await.is_ok()
            } else {
                false
            };

            let queued_peers = {
                let queue = self.pending_response_sends.lock().await;
                queue
                    .iter()
                    .map(|job| job.peer_id.clone())
                    .collect::<HashSet<_>>()
            };
            let mut stats = self.peer_wire_stats.write().await;
            let entry = stats.entry(selected.peer_id.clone()).or_default();
            if sent {
                entry.bytes_sent = entry.bytes_sent.saturating_add(selected.bytes.len() as u64);
                entry.bandwidth_debt = selected_finish;
            }
            if queued_peers.is_empty() {
                for peer_stats in stats.values_mut() {
                    peer_stats.bandwidth_debt = 0.0;
                }
            } else {
                let floor = queued_peers
                    .iter()
                    .filter_map(|peer_id| stats.get(peer_id).map(|peer| peer.bandwidth_debt))
                    .fold(f64::INFINITY, f64::min);
                if floor.is_finite() && floor > 0.0 {
                    for peer_id in queued_peers {
                        if let Some(peer_stats) = stats.get_mut(&peer_id) {
                            peer_stats.bandwidth_debt =
                                (peer_stats.bandwidth_debt - floor).max(0.0);
                        }
                    }
                }
            }
        }
    }

    fn deterministic_actor_draw_for(peer_id: &str, hash: &Hash, salt: u64) -> f64 {
        let mut hasher = DefaultHasher::new();
        peer_id.hash(&mut hasher);
        hash.hash(&mut hasher);
        salt.hash(&mut hasher);
        let v = hasher.finish();
        (v as f64) / (u64::MAX as f64)
    }

    fn deterministic_actor_draw(&self, hash: &Hash, salt: u64) -> f64 {
        Self::deterministic_actor_draw_for(self.signaling.peer_id(), hash, salt)
    }

    fn peer_metadata_pointer_slot_hash() -> Hash {
        hashtree_core::sha256(PEER_METADATA_POINTER_SLOT_KEY)
    }

    fn decode_hash_hex(hash_hex: &str) -> Result<Hash, StoreError> {
        let bytes = hex::decode(hash_hex)
            .map_err(|e| StoreError::Other(format!("Invalid hash hex: {e}")))?;
        if bytes.len() != 32 {
            return Err(StoreError::Other(format!(
                "Invalid hash length {}, expected 32 bytes",
                bytes.len()
            )));
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes);
        Ok(hash)
    }

    fn should_drop_response(&self, hash: &Hash) -> bool {
        let p = self.response_behavior().drop_response_prob;
        if p <= 0.0 {
            return false;
        }
        self.deterministic_actor_draw(hash, 0xD0_D0_D0_D0_D0_D0_D0_D0) < p
    }

    fn should_corrupt_response(&self, hash: &Hash) -> bool {
        let p = self.response_behavior().corrupt_response_prob;
        if p <= 0.0 {
            return false;
        }
        self.deterministic_actor_draw(hash, 0xC0_C0_C0_C0_C0_C0_C0_C0) < p
    }

    fn should_stall_response(&self, hash: &Hash) -> bool {
        let p = self.response_behavior().stall_response_prob;
        if p <= 0.0 {
            return false;
        }
        self.deterministic_actor_draw(hash, 0x5A_11_5A_11_5A_11_5A_11) < p
    }

    fn response_send_delay(&self, hash: &Hash, payload_len: usize) -> Duration {
        let behavior = self.response_behavior();
        let mut total_ms = behavior
            .extra_delay_ms
            .saturating_add(behavior.first_byte_delay_ms);

        if behavior.bytes_per_second > 0 && payload_len > 0 {
            let throughput_ms = ((payload_len as u128) * 1000)
                .div_ceil(behavior.bytes_per_second as u128)
                .min(u64::MAX as u128) as u64;
            total_ms = total_ms.saturating_add(throughput_ms);
        }

        if behavior.stall_delay_ms > 0 && self.should_stall_response(hash) {
            total_ms = total_ms.saturating_add(behavior.stall_delay_ms);
        }

        Duration::from_millis(total_ms)
    }

    async fn ordered_connected_peers(&self, exclude_peer_id: Option<&str>) -> Vec<String> {
        let current_peer_ids = self.signaling.peer_ids().await;
        if current_peer_ids.is_empty() {
            return Vec::new();
        }

        sync_selector_peers(&self.peer_selector, &current_peer_ids).await;
        let hash_get_peer_ids: HashSet<String> = self
            .signaling
            .hash_get_peer_ids()
            .await
            .into_iter()
            .collect();
        let mut candidate_peer_ids: Vec<String> = current_peer_ids
            .into_iter()
            .filter(|peer_id| hash_get_peer_ids.contains(peer_id))
            .filter(|peer_id| exclude_peer_id.is_none_or(|exclude| peer_id != exclude))
            .collect();
        if candidate_peer_ids.is_empty() {
            return Vec::new();
        }

        let current_set: HashSet<&str> = candidate_peer_ids.iter().map(String::as_str).collect();
        let mut selector = self.peer_selector.write().await;
        let mut selector_order = selector.select_peers();
        selector_order.retain(|peer_id| current_set.contains(peer_id.as_str()));
        if selector_order.is_empty() {
            let mut fallback = candidate_peer_ids;
            fallback.sort();
            return fallback;
        }
        let backed_off: HashMap<String, bool> = candidate_peer_ids
            .iter()
            .map(|peer_id| (peer_id.clone(), selector.is_peer_backed_off(peer_id)))
            .collect();
        drop(selector);

        let rank: HashMap<&str, usize> = selector_order
            .iter()
            .enumerate()
            .map(|(idx, peer_id)| (peer_id.as_str(), idx))
            .collect();
        let active = self.peer_active_requests.read().await;
        candidate_peer_ids.sort_by(|left, right| {
            let left_backed_off = backed_off.get(left).copied().unwrap_or(false);
            let right_backed_off = backed_off.get(right).copied().unwrap_or(false);
            if left_backed_off != right_backed_off {
                return if left_backed_off {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                };
            }
            let left_rank = rank.get(left.as_str()).copied().unwrap_or(usize::MAX / 2);
            let right_rank = rank.get(right.as_str()).copied().unwrap_or(usize::MAX / 2);
            let left_load = active.get(left).copied().unwrap_or(0);
            let right_load = active.get(right).copied().unwrap_or(0);
            (left_rank + left_load.saturating_mul(ACTIVE_PEER_REQUEST_RANK_PENALTY))
                .cmp(&(right_rank + right_load.saturating_mul(ACTIVE_PEER_REQUEST_RANK_PENALTY)))
                .then_with(|| left.cmp(right))
        });
        candidate_peer_ids
    }

    async fn reserve_peer_request(&self, peer_id: &str) {
        let mut active = self.peer_active_requests.write().await;
        *active.entry(peer_id.to_string()).or_insert(0) += 1;
    }

    async fn release_peer_request(&self, peer_id: &str) {
        let mut active = self.peer_active_requests.write().await;
        let Some(count) = active.get_mut(peer_id) else {
            return;
        };
        if *count <= 1 {
            active.remove(peer_id);
        } else {
            *count -= 1;
        }
    }

    async fn release_queried_peer_requests(&self, peer_ids: &[String]) {
        for peer_id in peer_ids {
            self.release_peer_request(peer_id).await;
        }
    }

    fn requested_quote_mint(&self) -> Option<&str> {
        if let Some(default_mint) = self.routing.cashu_default_mint.as_deref() {
            if self.routing.cashu_accepted_mints.is_empty()
                || self
                    .routing
                    .cashu_accepted_mints
                    .iter()
                    .any(|mint| mint == default_mint)
            {
                return Some(default_mint);
            }
        }

        self.routing
            .cashu_accepted_mints
            .first()
            .map(String::as_str)
    }

    fn choose_quote_mint(&self, requested_mint: Option<&str>) -> Option<String> {
        if let Some(requested_mint) = requested_mint {
            if self.accepts_quote_mint(Some(requested_mint)) {
                return Some(requested_mint.to_string());
            }
        }
        if let Some(default_mint) = self.routing.cashu_default_mint.as_ref() {
            return Some(default_mint.clone());
        }
        if let Some(first_mint) = self.routing.cashu_accepted_mints.first() {
            return Some(first_mint.clone());
        }
        requested_mint.map(str::to_string)
    }

    fn accepts_quote_mint(&self, mint_url: Option<&str>) -> bool {
        if self.routing.cashu_accepted_mints.is_empty() {
            return true;
        }

        let Some(mint_url) = mint_url else {
            return false;
        };
        self.routing
            .cashu_accepted_mints
            .iter()
            .any(|mint| mint == mint_url)
    }

    fn trusts_quote_mint(&self, mint_url: Option<&str>) -> bool {
        let Some(mint_url) = mint_url else {
            return self.routing.cashu_default_mint.is_none()
                && self.routing.cashu_accepted_mints.is_empty();
        };
        self.routing.cashu_default_mint.as_deref() == Some(mint_url)
            || self
                .routing
                .cashu_accepted_mints
                .iter()
                .any(|mint| mint == mint_url)
    }

    async fn peer_suggested_mint_cap_sat(&self, peer_id: &str) -> u64 {
        let base = self.routing.cashu_peer_suggested_mint_base_cap_sat;
        if base == 0 {
            return 0;
        }

        let selector = self.peer_selector.read().await;
        let Some(stats) = selector.get_stats(peer_id) else {
            let max_cap = self.routing.cashu_peer_suggested_mint_max_cap_sat;
            return if max_cap > 0 { base.min(max_cap) } else { base };
        };

        if stats.cashu_payment_defaults > 0
            && stats.cashu_payment_defaults >= stats.cashu_payment_receipts
        {
            return 0;
        }

        let success_bonus = stats
            .successes
            .saturating_mul(self.routing.cashu_peer_suggested_mint_success_step_sat);
        let receipt_bonus = stats
            .cashu_payment_receipts
            .saturating_mul(self.routing.cashu_peer_suggested_mint_receipt_step_sat);
        let mut cap = base
            .saturating_add(success_bonus)
            .saturating_add(receipt_bonus);
        let max_cap = self.routing.cashu_peer_suggested_mint_max_cap_sat;
        if max_cap > 0 {
            cap = cap.min(max_cap);
        }
        cap
    }

    async fn should_accept_quote_response(
        &self,
        from_peer: &str,
        preferred_mint_url: Option<&str>,
        offered_payment_sat: u64,
        res: &DataQuoteResponse,
    ) -> bool {
        let Some(payment_sat) = res.p else {
            return false;
        };
        if payment_sat > offered_payment_sat {
            return false;
        }

        let response_mint = res.m.as_deref();
        if response_mint == preferred_mint_url {
            return true;
        }
        if self.trusts_quote_mint(response_mint) {
            return true;
        }
        if response_mint.is_none() {
            return false;
        }

        payment_sat <= self.peer_suggested_mint_cap_sat(from_peer).await
    }

    async fn issue_quote(
        &self,
        peer_id: &str,
        hash_key: &str,
        payment_sat: u64,
        ttl_ms: u32,
        mint_url: Option<&str>,
    ) -> u64 {
        let quote_id = {
            let mut next = self.next_quote_id.write().await;
            let quote_id = *next;
            *next = next.saturating_add(1);
            quote_id
        };

        let expires_at = Instant::now() + Duration::from_millis(ttl_ms as u64);
        self.issued_quotes.write().await.insert(
            (peer_id.to_string(), hash_key.to_string(), quote_id),
            IssuedQuote {
                expires_at,
                payment_sat,
                mint_url: mint_url.map(str::to_string),
            },
        );
        quote_id
    }

    async fn take_valid_quote(&self, peer_id: &str, hash_key: &str, quote_id: u64) -> bool {
        let key = (peer_id.to_string(), hash_key.to_string(), quote_id);
        let Some(quote) = self.issued_quotes.write().await.remove(&key) else {
            return false;
        };
        quote.expires_at > Instant::now()
    }

    async fn send_request_to_peer(
        &self,
        peer_id: &str,
        hash: &Hash,
        request_htl: u8,
        quote_id: Option<u64>,
    ) -> bool {
        if !should_forward_htl(request_htl) {
            return false;
        }

        let channel = match self.signaling.get_channel(peer_id).await {
            Some(c) => c,
            None => return false,
        };

        // Hashtree owns HTL and consumes exactly one unit when forwarding a
        // blob request to another mesh peer. Transport/routing hops below this
        // layer must not alter it.
        let send_htl = request_htl.saturating_sub(1);
        let req = match quote_id {
            Some(quote_id) => create_request_with_quote(hash, send_htl, quote_id),
            None => create_request(hash, send_htl),
        };
        let request_bytes = encode_request(&req);
        let request_len = request_bytes.len() as u64;

        {
            let mut selector = self.peer_selector.write().await;
            selector.record_request(peer_id, request_len);
        }

        match channel.send(request_bytes).await {
            Ok(()) => {
                self.record_peer_wire_sent(peer_id, request_len).await;
                true
            }
            Err(_) => {
                self.peer_selector.write().await.record_failure(peer_id);
                false
            }
        }
    }

    async fn send_quote_request_to_peer(
        &self,
        peer_id: &str,
        hash: &Hash,
        payment_sat: u64,
        ttl_ms: u32,
        mint_url: Option<&str>,
    ) -> bool {
        let channel = match self.signaling.get_channel(peer_id).await {
            Some(c) => c,
            None => return false,
        };

        let req = create_quote_request(hash, ttl_ms, payment_sat, mint_url);
        let request_bytes = encode_quote_request(&req);
        let request_len = request_bytes.len() as u64;

        match channel.send(request_bytes).await {
            Ok(()) => {
                self.record_peer_wire_sent(peer_id, request_len).await;
                true
            }
            Err(_) => false,
        }
    }

    /// Get peer count
    pub async fn peer_count(&self) -> usize {
        self.signaling.peer_count().await
    }

    /// Get connected mesh peer IDs.
    pub async fn peer_ids(&self) -> Vec<String> {
        self.signaling.peer_ids().await
    }

    /// Check if we need more peers
    pub async fn needs_peers(&self) -> bool {
        self.signaling.needs_peers().await
    }

    /// Re-broadcast hello to refresh discovery as topology changes.
    pub async fn send_hello(&self) -> Result<(), TransportError> {
        self.signaling.send_hello(vec![]).await
    }

    /// Drain all currently available peer-link messages and handle them.
    ///
    /// This keeps the message pump logic shared between simulation and the
    /// default production wrapper instead of duplicating per-channel loops.
    pub async fn drain_available_data_messages(self: &Arc<Self>) -> DataPumpStats {
        let mut stats = DataPumpStats::default();
        let peer_ids = self.signaling.peer_ids().await;
        for peer_id in peer_ids {
            let Some(channel) = self.signaling.get_channel(&peer_id).await else {
                continue;
            };

            while let Some(data) = channel.try_recv() {
                stats.processed += 1;
                stats.processed_bytes += data.len() as u64;
                if let Some(msg) = parse_message(&data) {
                    match msg {
                        DataMessage::Request(_) => stats.request_messages += 1,
                        DataMessage::Response(_) => stats.response_messages += 1,
                        DataMessage::QuoteRequest(_) => stats.quote_request_messages += 1,
                        DataMessage::QuoteResponse(_) => stats.quote_response_messages += 1,
                        DataMessage::PubsubInterest(_) => stats.pubsub_interest_messages += 1,
                        DataMessage::PubsubFrame(_) => stats.pubsub_frame_messages += 1,
                        DataMessage::PubsubInventory(_) => stats.pubsub_inventory_messages += 1,
                        DataMessage::PubsubWant(_) => stats.pubsub_want_messages += 1,
                        DataMessage::Payment(_)
                        | DataMessage::PaymentAck(_)
                        | DataMessage::Chunk(_)
                        | DataMessage::PeerHints(_) => {}
                    }
                }
                self.handle_data_message(&peer_id, &data).await;
            }
        }
        stats
    }

    /// Apply an out-of-band payment credit to a peer's routing priority.
    pub async fn record_cashu_payment_for_peer(&self, peer_id: &str, amount_sat: u64) {
        self.peer_selector
            .write()
            .await
            .record_cashu_payment(peer_id, amount_sat);
    }

    /// Record a post-delivery payment we received from a peer.
    pub async fn record_cashu_receipt_from_peer(&self, peer_id: &str, amount_sat: u64) {
        self.peer_selector
            .write()
            .await
            .record_cashu_receipt(peer_id, amount_sat);
    }

    /// Record that a peer failed to pay after we delivered successfully.
    pub async fn record_cashu_payment_default_from_peer(&self, peer_id: &str) {
        self.peer_selector
            .write()
            .await
            .record_cashu_payment_default(peer_id);
    }

    /// Snapshot routing/selection summary for inspection/debugging.
    pub async fn selector_summary(&self) -> crate::peer_selector::SelectorSummary {
        self.peer_selector.read().await.summary()
    }

    fn should_refuse_requests_from_peer(&self, selector: &PeerSelector, peer_id: &str) -> bool {
        selector.is_peer_blocked_for_payment_defaults(
            peer_id,
            self.routing.cashu_payment_default_block_threshold,
        )
    }

    /// Export live peer metadata for inspection/debugging.
    pub async fn peer_metadata_snapshot(&self) -> PeerMetadataSnapshot {
        self.peer_selector
            .read()
            .await
            .export_peer_metadata_snapshot()
    }

    /// Snapshot current peer metadata and persist it into `local_store`.
    ///
    /// Uses content-addressed storage for the snapshot body and a reserved
    /// mutable pointer slot for the "latest snapshot hash".
    pub async fn persist_peer_metadata(&self) -> Result<Hash, StoreError> {
        let snapshot = self
            .peer_selector
            .read()
            .await
            .export_peer_metadata_snapshot();
        let bytes = serde_json::to_vec(&snapshot).map_err(|e| {
            StoreError::Other(format!("Failed to encode peer metadata snapshot: {e}"))
        })?;
        let snapshot_hash = hashtree_core::sha256(&bytes);
        let _ = self.local_store.put(snapshot_hash, bytes).await?;

        let pointer_slot = Self::peer_metadata_pointer_slot_hash();
        let pointer_bytes = hex::encode(snapshot_hash).into_bytes();
        let _ = self.local_store.delete(&pointer_slot).await?;
        let _ = self.local_store.put(pointer_slot, pointer_bytes).await?;

        Ok(snapshot_hash)
    }

    /// Load persisted peer metadata from `local_store` if available.
    pub async fn load_peer_metadata(&self) -> Result<bool, StoreError> {
        let pointer_slot = Self::peer_metadata_pointer_slot_hash();
        let Some(pointer_bytes) = self.local_store.get(&pointer_slot).await? else {
            return Ok(false);
        };
        let pointer_hex = std::str::from_utf8(&pointer_bytes).map_err(|e| {
            StoreError::Other(format!("Peer metadata pointer is not valid UTF-8: {e}"))
        })?;
        let snapshot_hash = Self::decode_hash_hex(pointer_hex.trim())?;

        let Some(snapshot_bytes) = self.local_store.get(&snapshot_hash).await? else {
            return Ok(false);
        };
        let snapshot: PeerMetadataSnapshot =
            serde_json::from_slice(&snapshot_bytes).map_err(|e| {
                StoreError::Other(format!("Failed to decode peer metadata snapshot: {e}"))
            })?;
        self.peer_selector
            .write()
            .await
            .import_peer_metadata_snapshot(&snapshot);
        Ok(true)
    }

    /// Request data from peers after negotiating a paid quote.
    ///
    /// If quote negotiation fails or the quoted peer does not deliver, the store
    /// falls back to the normal unpaid retrieval path to preserve liveness.
    pub async fn get_with_quote(
        &self,
        hash: &Hash,
        payment_sat: u64,
        quote_ttl: Duration,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        if let Some(data) = self.local_store.get(hash).await? {
            if hashtree_core::sha256(&data) != *hash {
                return Err(StoreError::Other(
                    "local store returned corrupt content".to_string(),
                ));
            }
            return Ok(Some(data));
        }
        self.request_from_peers_with_quote(hash, payment_sat, quote_ttl)
            .await
    }

    async fn request_from_peers_with_quote(
        &self,
        hash: &Hash,
        payment_sat: u64,
        quote_ttl: Duration,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let ordered_peer_ids = self.ordered_connected_peers(None).await;
        if ordered_peer_ids.is_empty() {
            return Ok(None);
        }

        if let Some(quote) = self
            .request_quote_from_peers(hash, payment_sat, quote_ttl, &ordered_peer_ids)
            .await
        {
            if let Some(data) = self
                .request_from_single_peer(hash, &quote.peer_id, MAX_HTL, Some(quote.quote_id))
                .await
            {
                return Ok(Some(data));
            }
        }

        match self
            .request_from_mesh_with_context(hash, &MeshReadContext::default())
            .await
        {
            RouteFetchOutcome::Hit(data) => Ok(Some(data)),
            RouteFetchOutcome::Miss => Ok(None),
            RouteFetchOutcome::Timeout => Err(StoreError::Other(
                "blob retrieval deadline expired before the search completed".to_string(),
            )),
        }
    }

    async fn request_quote_from_peers(
        &self,
        hash: &Hash,
        payment_sat: u64,
        quote_ttl: Duration,
        ordered_peer_ids: &[String],
    ) -> Option<NegotiatedQuote> {
        if ordered_peer_ids.is_empty() {
            return None;
        }
        let ttl_ms = quote_ttl.as_millis().min(u32::MAX as u128) as u32;
        if ttl_ms == 0 {
            return None;
        }
        let requested_mint = self.requested_quote_mint().map(str::to_string);

        let hash_key = hash_to_key(hash);
        let (tx, rx) = oneshot::channel();
        self.pending_quotes.write().await.insert(
            hash_key.clone(),
            PendingQuoteRequest {
                response_tx: tx,
                preferred_mint_url: requested_mint.clone(),
                offered_payment_sat: payment_sat,
            },
        );

        let rx = Arc::new(Mutex::new(rx));
        let result = run_hedged_waves(
            ordered_peer_ids.len(),
            self.routing.dispatch,
            self.request_timeout,
            |range| {
                let wave_peer_ids = ordered_peer_ids[range].to_vec();
                let requested_mint = requested_mint.clone();
                let hash = *hash;
                async move {
                    let mut sent = 0usize;
                    for peer_id in wave_peer_ids {
                        if self
                            .send_quote_request_to_peer(
                                &peer_id,
                                &hash,
                                payment_sat,
                                ttl_ms,
                                requested_mint.as_deref(),
                            )
                            .await
                        {
                            sent += 1;
                        }
                    }
                    sent
                }
            },
            |wait| {
                let rx = rx.clone();
                async move {
                    let mut rx = rx.lock().await;
                    match tokio::time::timeout(wait, &mut *rx).await {
                        Ok(Ok(Some(quote))) => HedgedWaveAction::Success(quote),
                        Ok(Ok(None)) | Ok(Err(_)) => HedgedWaveAction::Abort,
                        Err(_) => HedgedWaveAction::Continue,
                    }
                }
            },
        )
        .await;
        let _ = self.pending_quotes.write().await.remove(&hash_key);
        result
    }

    async fn register_pending_request(
        &self,
        request_key: PendingRequestKey,
        queried_peers: Vec<String>,
    ) -> (Arc<()>, oneshot::Receiver<Option<Vec<u8>>>) {
        let owner = Arc::new(());
        let (response_tx, response_rx) = oneshot::channel();
        self.pending_requests
            .write()
            .await
            .entry(request_key)
            .or_default()
            .push(PendingRequest {
                owner: owner.clone(),
                response_tx,
                started_at: Instant::now(),
                queried_peers,
            });
        (owner, response_rx)
    }

    async fn take_pending_request(
        &self,
        request_key: PendingRequestKey,
        owner: &Arc<()>,
    ) -> Option<(PendingRequest, bool)> {
        let mut pending = self.pending_requests.write().await;
        let (request, remove_key) = {
            let requests = pending.get_mut(&request_key)?;
            let index = requests
                .iter()
                .position(|request| Arc::ptr_eq(&request.owner, owner))?;
            let request = requests.swap_remove(index);
            (request, requests.is_empty())
        };
        if remove_key {
            pending.remove(&request_key);
        }
        Some((request, remove_key))
    }

    async fn request_from_single_peer(
        &self,
        hash: &Hash,
        peer_id: &str,
        request_htl: u8,
        quote_id: Option<u64>,
    ) -> Option<Vec<u8>> {
        let request_key = PendingRequestKey::new(*hash, request_htl);
        let (owner, rx) = self
            .register_pending_request(request_key, vec![peer_id.to_string()])
            .await;

        let mut rx = rx;
        if !self
            .send_request_to_peer(peer_id, hash, request_htl, quote_id)
            .await
        {
            if self
                .take_pending_request(request_key, &owner)
                .await
                .is_some_and(|(_, last)| last)
            {
                let _ = self.take_forward_requesters(request_key).await;
            }
            return None;
        }
        self.reserve_peer_request(peer_id).await;

        if let Ok(Ok(Some(data))) = tokio::time::timeout(self.request_timeout, &mut rx).await {
            if data.len() <= BLOB_MAX_BYTES && hashtree_core::sha256(&data) == *hash {
                let _ = self.local_store.put(*hash, data.clone()).await;
                return Some(data);
            }
        }

        if let Some((pending, last)) = self.take_pending_request(request_key, &owner).await {
            self.release_queried_peer_requests(&pending.queried_peers)
                .await;
            for peer_id in pending.queried_peers {
                self.peer_selector.write().await.record_timeout(&peer_id);
            }
            if last {
                let _ = self.take_forward_requesters(request_key).await;
            }
        }
        None
    }

    async fn request_from_ordered_peers(
        &self,
        hash: &Hash,
        ordered_peer_ids: &[String],
        request_htl: u8,
        timeout: Duration,
    ) -> RouteFetchOutcome {
        let request_key = PendingRequestKey::new(*hash, request_htl);
        let (owner, rx) = self.register_pending_request(request_key, Vec::new()).await;

        let rx = Arc::new(Mutex::new(rx));
        let result = run_hedged_waves(
            ordered_peer_ids.len(),
            normalize_dispatch_config(self.routing.dispatch, ordered_peer_ids.len()),
            timeout,
            |range| {
                let wave_peer_ids = ordered_peer_ids[range].to_vec();
                let hash = *hash;
                let owner = owner.clone();
                async move {
                    let mut sent = 0usize;
                    for peer_id in wave_peer_ids {
                        if self
                            .send_request_to_peer(&peer_id, &hash, request_htl, None)
                            .await
                        {
                            sent += 1;
                            self.reserve_peer_request(&peer_id).await;
                            if let Some(pending) = self
                                .pending_requests
                                .write()
                                .await
                                .get_mut(&request_key)
                                .and_then(|requests| {
                                    requests
                                        .iter_mut()
                                        .find(|request| Arc::ptr_eq(&request.owner, &owner))
                                })
                            {
                                pending.queried_peers.push(peer_id);
                            }
                        }
                    }
                    sent
                }
            },
            |wait| {
                let rx = rx.clone();
                async move {
                    let mut rx = rx.lock().await;
                    match tokio::time::timeout(wait, &mut *rx).await {
                        Ok(Ok(Some(data)))
                            if data.len() <= BLOB_MAX_BYTES
                                && hashtree_core::sha256(&data) == *hash =>
                        {
                            HedgedWaveAction::Success(data)
                        }
                        Ok(Ok(Some(_))) => HedgedWaveAction::Continue,
                        Ok(Ok(None)) | Ok(Err(_)) => HedgedWaveAction::Abort,
                        Err(_) => HedgedWaveAction::Continue,
                    }
                }
            },
        )
        .await;

        let Some(data) = result else {
            if let Some((pending, last)) = self.take_pending_request(request_key, &owner).await {
                self.release_queried_peer_requests(&pending.queried_peers)
                    .await;
                for peer_id in pending.queried_peers {
                    self.peer_selector.write().await.record_timeout(&peer_id);
                }
                if last {
                    let _ = self.take_forward_requesters(request_key).await;
                }
            }
            return RouteFetchOutcome::Timeout;
        };

        let _ = self.local_store.put(*hash, data.clone()).await;
        RouteFetchOutcome::Hit(data)
    }

    async fn request_from_mesh_with_context(
        &self,
        hash: &Hash,
        context: &MeshReadContext,
    ) -> RouteFetchOutcome {
        if !should_forward_htl(context.request_htl) {
            return RouteFetchOutcome::Miss;
        }
        let mut peers = self
            .ordered_connected_peers(context.exclude_peer_id.as_deref())
            .await;
        if let Some(attempt_budget) = context.attempt_budget {
            peers.truncate(attempt_budget);
        }
        if peers.is_empty() {
            return RouteFetchOutcome::Miss;
        }
        let timeout = context
            .deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(self.request_timeout)
            .min(self.request_timeout);
        if timeout.is_zero() {
            return RouteFetchOutcome::Timeout;
        }
        self.request_from_ordered_peers(hash, &peers, context.request_htl, timeout)
            .await
    }

    async fn begin_forward_request(
        &self,
        request_key: PendingRequestKey,
        requester_id: &str,
    ) -> bool {
        let mut pending = self.pending_forward_requests.write().await;
        if let Some(existing) = pending.get_mut(&request_key) {
            existing.requester_ids.insert(requester_id.to_string());
            return false;
        }

        let mut requester_ids = HashSet::new();
        requester_ids.insert(requester_id.to_string());
        pending.insert(request_key, PendingForwardRequest { requester_ids });
        true
    }

    async fn take_forward_requesters(&self, request_key: PendingRequestKey) -> Vec<String> {
        self.pending_forward_requests
            .write()
            .await
            .remove(&request_key)
            .map(|pending| pending.requester_ids.into_iter().collect())
            .unwrap_or_default()
    }

    async fn complete_pending_response(
        self: &Arc<Self>,
        from_peer: &str,
        hash: &Hash,
        payload: Vec<u8>,
    ) {
        let pending = {
            let mut requests = self.pending_requests.write().await;
            let matching_keys: Vec<_> = requests
                .keys()
                .filter(|key| key.hash == *hash)
                .copied()
                .collect();
            matching_keys
                .into_iter()
                .flat_map(|key| {
                    requests
                        .remove(&key)
                        .into_iter()
                        .flatten()
                        .map(move |request| (key, request))
                })
                .collect::<Vec<_>>()
        };
        if pending.is_empty() {
            return;
        }

        let payload_bytes = payload.len() as u64;
        self.record_useful_bytes_received_from_peer(from_peer, payload_bytes)
            .await;
        {
            let mut deliveries = self.verified_block_deliveries.lock().await;
            deliveries.deliveries.push_back(VerifiedBlockDelivery {
                hash: *hash,
                provider_peer_id: from_peer.to_string(),
                payload_bytes,
            });
            while deliveries.deliveries.len() > VERIFIED_BLOCK_DELIVERY_CAPACITY {
                deliveries.deliveries.pop_front();
                deliveries.dropped_since_last_drain =
                    deliveries.dropped_since_last_drain.saturating_add(1);
            }
        }

        let rtt_ms = pending
            .iter()
            .map(|(_, request)| request.started_at.elapsed().as_millis() as u64)
            .max()
            .unwrap_or_default();
        let queried_peers: Vec<_> = pending
            .iter()
            .flat_map(|(_, request)| request.queried_peers.iter().cloned())
            .collect();
        self.release_queried_peer_requests(&queried_peers).await;
        self.peer_selector
            .write()
            .await
            .record_success(from_peer, rtt_ms, payload_bytes);

        let mut forward_requesters = HashSet::new();
        for (request_key, pending) in pending {
            forward_requesters.extend(self.take_forward_requesters(request_key).await);
            let _ = pending.response_tx.send(Some(payload.clone()));
        }
        if !forward_requesters.is_empty() {
            let response_bytes = encode_response(&create_response(hash, payload));
            for requester_id in forward_requesters {
                Arc::clone(self)
                    .enqueue_response_send(requester_id, response_bytes.clone(), Instant::now())
                    .await;
            }
        }
    }

    async fn handle_quote_response_message(&self, from_peer: &str, res: DataQuoteResponse) {
        if !res.a {
            return;
        }

        let Some(quote_id) = res.q else {
            return;
        };

        let hash_key = hash_to_key(&res.h);
        let (preferred_mint_url, offered_payment_sat) = {
            let pending_quotes = self.pending_quotes.read().await;
            let Some(pending) = pending_quotes.get(&hash_key) else {
                return;
            };
            (
                pending.preferred_mint_url.clone(),
                pending.offered_payment_sat,
            )
        };
        if !self
            .should_accept_quote_response(
                from_peer,
                preferred_mint_url.as_deref(),
                offered_payment_sat,
                &res,
            )
            .await
        {
            return;
        }
        let mut pending_quotes = self.pending_quotes.write().await;
        if let Some(pending) = pending_quotes.remove(&hash_key) {
            let _ = pending.response_tx.send(Some(NegotiatedQuote {
                peer_id: from_peer.to_string(),
                quote_id,
                mint_url: res.m,
            }));
        }
    }

    async fn handle_response_message(
        self: &Arc<Self>,
        from_peer: &str,
        res: crate::protocol::DataResponse,
    ) {
        let hash_key = hash_to_key(&res.h);
        let hash = match crate::protocol::bytes_to_hash(&res.h) {
            Some(h) => h,
            None => return,
        };

        // Ignore malformed/corrupt payload and keep waiting for a valid response.
        if hashtree_core::sha256(&res.d) != hash {
            self.peer_selector.write().await.record_failure(from_peer);
            if self.debug {
                println!("[MeshStoreCore] Ignoring invalid response payload for {hash_key}");
            }
            return;
        }

        self.complete_pending_response(from_peer, &hash, res.d)
            .await;
    }

    async fn handle_quote_request_message(&self, from_peer: &str, req: DataQuoteRequest) {
        let hash = match crate::protocol::bytes_to_hash(&req.h) {
            Some(h) => h,
            None => return,
        };
        let hash_key = hash_to_key(&hash);

        {
            let selector = self.peer_selector.read().await;
            if self.should_refuse_requests_from_peer(&selector, from_peer) {
                if self.debug {
                    println!(
                        "[MeshStoreCore] Refusing quote request from delinquent peer {}",
                        from_peer
                    );
                }
                return;
            }
        }

        let chosen_mint = self.choose_quote_mint(req.m.as_deref());
        let can_serve = self.local_store.has(&hash).await.ok().unwrap_or(false)
            && !self.should_drop_response(&hash)
            && !self.should_corrupt_response(&hash);

        let res = if can_serve {
            let quote_id = self
                .issue_quote(from_peer, &hash_key, req.p, req.t, chosen_mint.as_deref())
                .await;
            create_quote_response_available(&hash, quote_id, req.p, req.t, chosen_mint.as_deref())
        } else {
            create_quote_response_unavailable(&hash)
        };
        let response_bytes = encode_quote_response(&res);
        if let Some(channel) = self.signaling.get_channel(from_peer).await {
            if channel.send(response_bytes.clone()).await.is_ok() {
                self.record_peer_wire_sent(from_peer, response_bytes.len() as u64)
                    .await;
            }
        }
    }

    async fn handle_request_message(
        self: &Arc<Self>,
        from_peer: &str,
        req: crate::protocol::DataRequest,
    ) {
        if req.htl > MAX_HTL {
            return;
        }
        let hash = match crate::protocol::bytes_to_hash(&req.h) {
            Some(h) => h,
            None => return,
        };
        let hash_key = hash_to_key(&hash);
        let request_key = PendingRequestKey::new(hash, req.htl);

        if let Some(quote_id) = req.q {
            if !self.take_valid_quote(from_peer, &hash_key, quote_id).await {
                if self.debug {
                    println!(
                        "[MeshStoreCore] Refusing request with invalid or expired quote {} from {}",
                        quote_id, from_peer
                    );
                }
                return;
            }
        }

        let allow_peer_forwarding = {
            let selector = self.peer_selector.read().await;
            !self.should_refuse_requests_from_peer(&selector, from_peer)
        };

        // Check local store
        if let Ok(Some(mut data)) = self.local_store.get(&hash).await {
            if data.len() <= BLOB_MAX_BYTES && hashtree_core::sha256(&data) == hash {
                if self.should_drop_response(&hash) {
                    if self.debug {
                        println!(
                            "[MeshStoreCore] Dropping response for {} due to actor profile",
                            hash_to_key(&hash)
                        );
                    }
                    return;
                }

                let response_delay = self.response_send_delay(&hash, data.len());
                if self.should_corrupt_response(&hash) {
                    if data.is_empty() {
                        data.push(0x80);
                    } else {
                        data[0] ^= 0x80;
                    }
                }

                let res = create_response(&hash, data);
                let response_bytes = encode_response(&res);
                let ready_at = Instant::now() + response_delay;
                Arc::clone(self)
                    .enqueue_response_send(from_peer.to_string(), response_bytes, ready_at)
                    .await;
                return;
            }
        }

        if self
            .pending_requests
            .read()
            .await
            .contains_key(&request_key)
        {
            let _ = self.begin_forward_request(request_key, from_peer).await;
            return;
        }

        if !self.begin_forward_request(request_key, from_peer).await {
            return;
        }

        let from_peer = from_peer.to_string();
        let this = Arc::clone(self);
        let request_htl = req.htl;
        tokio::spawn(async move {
            let result = if allow_peer_forwarding {
                let context = MeshReadContext {
                    exclude_peer_id: Some(from_peer.clone()),
                    request_htl,
                    deadline: None,
                    attempt_budget: None,
                };
                this.request_from_mesh_with_context(&hash, &context).await
            } else {
                if this.debug {
                    println!(
                        "[MeshStoreCore] Refusing to forward request from delinquent peer {}",
                        from_peer
                    );
                }
                RouteFetchOutcome::Miss
            };
            let requester_ids = this.take_forward_requesters(request_key).await;
            match result {
                RouteFetchOutcome::Hit(data) => {
                    let ready_at = Instant::now() + this.response_send_delay(&hash, data.len());
                    let res = create_response(&hash, data);
                    let response_bytes = encode_response(&res);
                    for requester_id in requester_ids {
                        Arc::clone(&this)
                            .enqueue_response_send(requester_id, response_bytes.clone(), ready_at)
                            .await;
                    }
                }
                RouteFetchOutcome::Miss | RouteFetchOutcome::Timeout => {}
            }
        });
    }

    async fn handle_pubsub_interest_message(
        self: &Arc<Self>,
        from_peer: &str,
        mut interest: PubsubInterest,
    ) {
        if !self.apply_pubsub_interest_route(from_peer, &interest).await {
            return;
        }

        if !self.routing.pubsub_forwarding || interest.htl <= 1 {
            return;
        }
        interest.htl = interest.htl.saturating_sub(1);
        let _ = self
            .send_pubsub_interest_to_peers(&interest, Some(from_peer))
            .await;
    }

    async fn handle_pubsub_frame_message(
        self: &Arc<Self>,
        from_peer: &str,
        mut frame: PubsubFrame,
        wire_bytes: usize,
    ) {
        if frame.stream_id.is_empty() || frame.origin_peer_id.is_empty() {
            return;
        }
        if frame.origin_peer_id == self.signaling.peer_id() {
            return;
        }

        let frame_key = Self::pubsub_frame_key(&frame);
        if !self
            .pubsub_seen_frames
            .lock()
            .await
            .insert_if_new(frame_key.clone())
        {
            return;
        }
        self.cache_pubsub_frame(frame_key.clone(), frame.clone())
            .await;

        let local_interested = self
            .pubsub_local_interests
            .read()
            .await
            .contains(&frame.stream_id);
        let mut downstream_peers = if self.routing.pubsub_forwarding && frame.htl > 1 {
            match self.routing.pubsub_delivery_mode {
                PubsubDeliveryMode::InterestPush => {
                    let mut peers = self
                        .interested_pubsub_peers(&frame.stream_id, Some(from_peer))
                        .await;
                    peers.extend(
                        self.take_pubsub_want_peers(&frame_key, Some(from_peer))
                            .await,
                    );
                    peers.sort();
                    peers.dedup();
                    peers
                }
                PubsubDeliveryMode::HtlInvWant => {
                    self.take_pubsub_want_peers(&frame_key, Some(from_peer))
                        .await
                }
            }
        } else {
            Vec::new()
        };
        downstream_peers.retain(|peer_id| peer_id != from_peer);

        if local_interested || !downstream_peers.is_empty() {
            self.record_useful_bytes_received_from_peer(from_peer, wire_bytes as u64)
                .await;
        }

        if local_interested {
            self.enqueue_pubsub_event(PubsubEvent {
                stream_id: frame.stream_id.clone(),
                seq: frame.seq,
                origin_peer_id: frame.origin_peer_id.clone(),
                from_peer_id: from_peer.to_string(),
                payload: frame.payload.clone(),
            })
            .await;
        }

        if downstream_peers.is_empty() {
            return;
        }

        frame.htl = frame.htl.saturating_sub(1);
        let _ = self
            .send_pubsub_frame_to_peers(&frame, &downstream_peers)
            .await;
    }

    async fn handle_pubsub_inventory_message(
        self: &Arc<Self>,
        from_peer: &str,
        inv: PubsubInventory,
        wire_bytes: usize,
    ) {
        if inv.stream_id.is_empty() || inv.origin_peer_id.is_empty() {
            return;
        }
        if inv.origin_peer_id == self.signaling.peer_id() {
            return;
        }

        let key = Self::pubsub_key(&inv.origin_peer_id, &inv.stream_id, inv.seq);
        if !self
            .pubsub_seen_inventories
            .lock()
            .await
            .insert_if_new(key.clone())
        {
            return;
        }
        {
            let mut routes = self.pubsub_inventory_routes.write().await;
            routes
                .entry(key.clone())
                .or_insert_with(|| from_peer.to_string());
        }

        let local_interested = self
            .pubsub_local_interests
            .read()
            .await
            .contains(&inv.stream_id);
        let downstream_peers = if self.routing.pubsub_forwarding {
            self.interested_pubsub_peers(&inv.stream_id, Some(from_peer))
                .await
        } else {
            Vec::new()
        };
        if local_interested || !downstream_peers.is_empty() {
            self.record_useful_bytes_received_from_peer(from_peer, wire_bytes as u64)
                .await;
            let want =
                create_pubsub_want(inv.stream_id.clone(), inv.seq, inv.origin_peer_id.clone());
            let _ = self.send_pubsub_want_upstream(&key, &want, None).await;
        }

        if !self.routing.pubsub_forwarding
            || downstream_peers.is_empty()
            || !should_forward_htl(inv.htl)
        {
            return;
        }
        let _ = self
            .send_pubsub_inventory_to_peers(&inv, &downstream_peers)
            .await;
    }

    async fn handle_pubsub_want_message(
        self: &Arc<Self>,
        from_peer: &str,
        want: PubsubWant,
        wire_bytes: usize,
    ) {
        if want.stream_id.is_empty() || want.origin_peer_id.is_empty() {
            return;
        }
        if want.origin_peer_id == from_peer {
            return;
        }

        let key = Self::pubsub_key(&want.origin_peer_id, &want.stream_id, want.seq);
        let want_key = format!("{from_peer}:{key}");
        if !self.pubsub_seen_wants.lock().await.insert_if_new(want_key) {
            return;
        }

        if let Some(frame) = self.cached_pubsub_frame(&key).await {
            self.record_useful_bytes_received_from_peer(from_peer, wire_bytes as u64)
                .await;
            let peers = vec![from_peer.to_string()];
            let _ = self.send_pubsub_frame_to_peers(&frame, &peers).await;
            return;
        }

        let has_upstream_route = self.pubsub_inventory_routes.read().await.contains_key(&key);
        if !has_upstream_route {
            return;
        }

        if self.remember_pubsub_want_peer(key.clone(), from_peer).await {
            self.record_useful_bytes_received_from_peer(from_peer, wire_bytes as u64)
                .await;
        }
        let _ = self
            .send_pubsub_want_upstream(&key, &want, Some(from_peer))
            .await;
    }

    /// Handle incoming data message
    pub async fn handle_data_message(self: &Arc<Self>, from_peer: &str, data: &[u8]) {
        self.record_peer_wire_received(from_peer, data.len() as u64)
            .await;
        let parsed = match parse_message(data) {
            Some(m) => m,
            None => return,
        };

        match parsed {
            DataMessage::Request(req) => {
                self.handle_request_message(from_peer, req).await;
            }
            DataMessage::Response(res) => {
                self.handle_response_message(from_peer, res).await;
            }
            DataMessage::QuoteRequest(req) => {
                self.handle_quote_request_message(from_peer, req).await;
            }
            DataMessage::QuoteResponse(res) => {
                self.handle_quote_response_message(from_peer, res).await;
            }
            DataMessage::PubsubInterest(interest) => {
                self.handle_pubsub_interest_message(from_peer, interest)
                    .await;
            }
            DataMessage::PubsubFrame(frame) => {
                self.handle_pubsub_frame_message(from_peer, frame, data.len())
                    .await;
            }
            DataMessage::PubsubInventory(inv) => {
                self.handle_pubsub_inventory_message(from_peer, inv, data.len())
                    .await;
            }
            DataMessage::PubsubWant(want) => {
                self.handle_pubsub_want_message(from_peer, want, data.len())
                    .await;
            }
            DataMessage::Payment(_)
            | DataMessage::PaymentAck(_)
            | DataMessage::Chunk(_)
            | DataMessage::PeerHints(_) => {}
        }
    }
}

#[async_trait]
impl<S, R, F> BlobRoute for MeshStoreCore<S, R, F>
where
    S: Store + Send + Sync + 'static,
    R: SignalingTransport + Send + Sync + 'static,
    F: PeerLinkFactory + Send + Sync + 'static,
{
    async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError> {
        self.route_with_context(
            request,
            BlobRouteContext {
                deadline: (Instant::now() + self.request_timeout).into(),
                attempt_budget: self.routing.dispatch.max_fanout.max(1),
            },
        )
        .await
    }

    async fn route_with_context(
        &self,
        request: BlobRequest,
        route_context: BlobRouteContext,
    ) -> Result<BlobReply, StoreError> {
        if request.htl > MAX_HTL {
            return Err(StoreError::Other(format!(
                "Hashtree blob HTL {} exceeds the maximum of {MAX_HTL}",
                request.htl
            )));
        }
        if let Some(data) = self.local_store.get(&request.hash).await? {
            if data.len() > BLOB_MAX_BYTES {
                return Err(StoreError::Other(format!(
                    "local store returned {} bytes, exceeding the {BLOB_MAX_BYTES}-byte limit",
                    data.len()
                )));
            }
            if hashtree_core::sha256(&data) != request.hash {
                return Err(StoreError::Other(
                    "local store returned corrupt content".to_string(),
                ));
            }
            return Ok(BlobReply::Data(data));
        }

        if request.htl == 0 {
            return Ok(BlobReply::NoResult);
        }

        let context = MeshReadContext {
            exclude_peer_id: None,
            request_htl: request.htl,
            deadline: Some(route_context.deadline.into()),
            attempt_budget: Some(route_context.attempt_budget),
        };
        match self
            .request_from_mesh_with_context(&request.hash, &context)
            .await
        {
            RouteFetchOutcome::Hit(data) => {
                if data.len() > BLOB_MAX_BYTES {
                    return Err(StoreError::Other(format!(
                        "blob route returned {} bytes, exceeding the {BLOB_MAX_BYTES}-byte limit",
                        data.len()
                    )));
                }
                if hashtree_core::sha256(&data) != request.hash {
                    return Err(StoreError::Other(
                        "blob route returned corrupt content".to_string(),
                    ));
                }
                Ok(BlobReply::Data(data))
            }
            RouteFetchOutcome::Miss => Ok(BlobReply::NoResult),
            RouteFetchOutcome::Timeout => Err(StoreError::Other(
                "blob retrieval deadline expired before the search completed".to_string(),
            )),
        }
    }
}

#[async_trait]
impl<S, R, F> Store for MeshStoreCore<S, R, F>
where
    S: Store + Send + Sync + 'static,
    R: SignalingTransport + Send + Sync + 'static,
    F: PeerLinkFactory + Send + Sync + 'static,
{
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.local_store.put(hash, data).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(
            match self
                .route(BlobRequest {
                    hash: *hash,
                    htl: MAX_HTL,
                })
                .await?
            {
                BlobReply::Data(data) => Some(data),
                BlobReply::NoResult => None,
            },
        )
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.local_store.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.local_store.delete(hash).await
    }
}

#[cfg(test)]
mod delivery_tests;

#[cfg(test)]
mod tests;

/// Type alias for simulation store.
pub type SimMeshStore<S> =
    MeshStoreCore<S, crate::mock::MockRelayTransport, crate::mock::MockConnectionFactory>;
