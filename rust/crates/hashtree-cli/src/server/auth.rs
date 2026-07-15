use crate::blob_cache::BlobCache;
use crate::fips_transport::{DaemonBlobResolver, DaemonFipsTransport};
use crate::nostr_relay::NostrRelay;
use crate::socialgraph;
use crate::storage::HashtreeStore;
use crate::webrtc::{PeerRootEvent, WebRTCState};
use axum::{
    body::Body,
    extract::ws::Message,
    extract::State,
    http::{header, HeaderMap, Request, Response, StatusCode},
    middleware::Next,
};
use futures::future::{BoxFuture, Shared};
use hashtree_core::{Cid, LinkType, TreeEntry};
use lru::LruCache;
use nostr::Keys;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::sync::{
    atomic::{AtomicU32, AtomicU64, Ordering},
    Arc, Mutex as StdMutex,
};
use std::time::{Duration, Instant};
use tokio::{
    sync::{mpsc, watch, Mutex, Semaphore},
    task::JoinHandle,
};

const LOOKUP_CACHE_CAPACITY: usize = 4096;
const LOOKUP_CACHE_HIT_TTL: Duration = Duration::from_secs(300);
const LOOKUP_CACHE_MISS_TTL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub enum LookupResult<T> {
    Hit(T),
    Miss,
}

impl<T> LookupResult<T> {
    pub fn from_option(value: Option<T>) -> Self {
        match value {
            Some(value) => Self::Hit(value),
            None => Self::Miss,
        }
    }

    pub fn into_option(self) -> Option<T> {
        match self {
            Self::Hit(value) => Some(value),
            Self::Miss => None,
        }
    }

    pub fn ttl(&self) -> Duration {
        match self {
            Self::Hit(_) => LOOKUP_CACHE_HIT_TTL,
            Self::Miss => LOOKUP_CACHE_MISS_TTL,
        }
    }
}

pub struct TimedLruCache<K, V> {
    cache: LruCache<K, TimedValue<V>>,
}

#[derive(Clone)]
struct TimedValue<V> {
    value: V,
    expires_at: Instant,
}

impl<K: Eq + Hash, V: Clone> TimedLruCache<K, V> {
    pub fn new(capacity: usize) -> Self {
        Self {
            cache: LruCache::new(NonZeroUsize::new(capacity.max(1)).unwrap()),
        }
    }

    pub fn get_cloned(&mut self, key: &K) -> Option<V> {
        let now = Instant::now();
        if let Some(entry) = self.cache.get(key) {
            if entry.expires_at > now {
                return Some(entry.value.clone());
            }
        }
        self.cache.pop(key);
        None
    }

    pub fn put(&mut self, key: K, value: V, ttl: Duration) {
        self.cache.put(
            key,
            TimedValue {
                value,
                expires_at: Instant::now() + ttl,
            },
        );
    }
}

pub fn new_lookup_cache<K: Eq + Hash, V: Clone>() -> TimedLruCache<K, V> {
    TimedLruCache::new(LOOKUP_CACHE_CAPACITY)
}

#[derive(Debug, Clone)]
pub struct CachedResolvedPathEntry {
    pub cid: Cid,
    pub link_type: LinkType,
}

#[derive(Debug, Clone)]
pub struct CachedTreeRootEntry {
    pub cid: Cid,
    pub source: &'static str,
    pub root_event: Option<PeerRootEvent>,
    pub event: Option<nostr::Event>,
    pub cached_at: Instant,
}

pub type SharedBlobFetch = Shared<BoxFuture<'static, bool>>;
pub type SharedBlobRead = Shared<BoxFuture<'static, Result<Option<Vec<u8>>, String>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsProtocol {
    HashtreeJson,
    HashtreeMsgpack,
    Unknown,
}

pub struct PendingRequest {
    pub origin_id: u64,
    pub hash: String,
    pub found: bool,
    pub origin_protocol: WsProtocol,
}

pub struct UpstreamNostrSubscription {
    pub close_tx: watch::Sender<bool>,
    pub tasks: Vec<JoinHandle<()>>,
}

#[derive(Debug, Clone, Default)]
pub struct UpstreamBlossomFetchSnapshot {
    pub lookup_attempts: u64,
    pub hits: u64,
    pub hit_bytes: u64,
    pub explicit_misses: u64,
    pub indeterminate_misses: u64,
    pub miss_cache_hits: u64,
    pub last_indeterminate_reason: Option<String>,
}

#[derive(Default)]
pub struct UpstreamBlossomFetchMetrics {
    lookup_attempts: AtomicU64,
    hits: AtomicU64,
    hit_bytes: AtomicU64,
    explicit_misses: AtomicU64,
    indeterminate_misses: AtomicU64,
    miss_cache_hits: AtomicU64,
    last_indeterminate_reason: StdMutex<Option<String>>,
}

impl UpstreamBlossomFetchMetrics {
    pub fn note_lookup_attempt(&self) {
        self.lookup_attempts.fetch_add(1, Ordering::Relaxed);
    }

    pub fn note_hit(&self, bytes: usize) {
        self.hits.fetch_add(1, Ordering::Relaxed);
        self.hit_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn note_explicit_miss(&self) {
        self.explicit_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn note_indeterminate_miss(&self, reason: impl Into<String>) {
        self.indeterminate_misses.fetch_add(1, Ordering::Relaxed);
        let reason = reason.into();
        if let Ok(mut last_reason) = self.last_indeterminate_reason.lock() {
            *last_reason = Some(reason.chars().take(512).collect());
        }
    }

    pub fn note_miss_cache_hit(&self) {
        self.miss_cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> UpstreamBlossomFetchSnapshot {
        UpstreamBlossomFetchSnapshot {
            lookup_attempts: self.lookup_attempts.load(Ordering::Relaxed),
            hits: self.hits.load(Ordering::Relaxed),
            hit_bytes: self.hit_bytes.load(Ordering::Relaxed),
            explicit_misses: self.explicit_misses.load(Ordering::Relaxed),
            indeterminate_misses: self.indeterminate_misses.load(Ordering::Relaxed),
            miss_cache_hits: self.miss_cache_hits.load(Ordering::Relaxed),
            last_indeterminate_reason: self
                .last_indeterminate_reason
                .lock()
                .ok()
                .and_then(|reason| reason.clone()),
        }
    }
}

pub struct WsRelayState {
    pub clients: Mutex<HashMap<u64, mpsc::UnboundedSender<Message>>>,
    pub pending: Mutex<HashMap<(u64, u32), PendingRequest>>,
    pub client_protocols: Mutex<HashMap<u64, WsProtocol>>,
    pub upstream_nostr_subscriptions: Mutex<HashMap<(u64, String), UpstreamNostrSubscription>>,
    pub upstream_seen_events: Mutex<HashMap<(u64, String), HashSet<String>>>,
    pub upstream_pending_eose: Mutex<HashMap<(u64, String), usize>>,
    pub next_client_id: AtomicU64,
    pub next_request_id: AtomicU32,
    pub upstream_relay_bytes_sent: AtomicU64,
    pub upstream_relay_bytes_received: AtomicU64,
}

impl WsRelayState {
    pub fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            client_protocols: Mutex::new(HashMap::new()),
            upstream_nostr_subscriptions: Mutex::new(HashMap::new()),
            upstream_seen_events: Mutex::new(HashMap::new()),
            upstream_pending_eose: Mutex::new(HashMap::new()),
            next_client_id: AtomicU64::new(1),
            next_request_id: AtomicU32::new(1),
            upstream_relay_bytes_sent: AtomicU64::new(0),
            upstream_relay_bytes_received: AtomicU64::new(0),
        }
    }

    pub fn next_id(&self) -> u64 {
        self.next_client_id.fetch_add(1, Ordering::SeqCst)
    }

    pub fn next_request_id(&self) -> u32 {
        self.next_request_id.fetch_add(1, Ordering::SeqCst)
    }

    pub fn note_upstream_relay_send(&self, bytes: usize) {
        self.upstream_relay_bytes_sent
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn note_upstream_relay_receive(&self, bytes: usize) {
        self.upstream_relay_bytes_received
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn upstream_relay_bandwidth(&self) -> (u64, u64) {
        (
            self.upstream_relay_bytes_sent.load(Ordering::Relaxed),
            self.upstream_relay_bytes_received.load(Ordering::Relaxed),
        )
    }
}

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<HashtreeStore>,
    pub auth: Option<AuthCredentials>,
    /// Unix timestamp when this daemon state was created.
    pub daemon_started_at: u64,
    pub peer_mode: crate::config::ServerMode,
    pub hash_get_enabled: bool,
    /// Whether HTTP cache misses should ask connected WebRTC peers before
    /// falling back to upstream Blossom.
    pub http_webrtc_fetch: bool,
    /// WebRTC peer state for forwarding requests to connected P2P peers
    pub webrtc_peers: Option<Arc<WebRTCState>>,
    /// FIPS-backed Hashtree blob transport for peer fetches and responses.
    pub fips_transport: Option<Arc<DaemonFipsTransport>>,
    /// Canonical Hashtree resolver whose peer routes run over fips-tcp.
    pub fips_blob_resolver: Option<Arc<DaemonBlobResolver>>,
    pub fetch_from_fips_peers: bool,
    /// WebSocket relay state for /ws clients
    pub ws_relay: Arc<WsRelayState>,
    /// Maximum upload size in bytes for Blossom uploads (default: 5 MB)
    pub max_upload_bytes: usize,
    /// Allow anyone with valid Nostr auth to write (default: true)
    /// When false, only allowed_pubkeys can write
    pub public_writes: bool,
    /// Allow public plaintext reads from mutable npub routes (default: false)
    /// When false, only allowed_pubkeys or social graph approved pubkeys can read.
    pub public_plaintext_reads: bool,
    /// Require untrusted cached blob ingress to look like encrypted CHK blobs.
    pub require_random_untrusted_ingest: bool,
    /// Return from Blossom upload after validation while storage writes finish in
    /// a bounded background queue.
    pub optimistic_blossom_uploads: bool,
    /// Background upload queue byte budget. Each queued body holds one permit per
    /// byte until the storage write completes.
    pub optimistic_upload_queue_bytes: usize,
    pub optimistic_upload_queue: Arc<Semaphore>,
    /// Pubkeys allowed to write (hex format, from config allowed_npubs)
    pub allowed_pubkeys: HashSet<String>,
    /// Upstream Blossom servers for cascade fetching
    pub upstream_blossom: Vec<String>,
    /// Shared HTTP client for upstream Blossom reads, so cold cache misses can
    /// reuse connections instead of rebuilding a client per blob.
    pub upstream_http_client: reqwest::Client,
    /// Short cache for explicit upstream Blossom 404 misses. This only records
    /// HTTP absence from configured Blossom upstreams; peer timeouts are not
    /// treated as absence.
    pub upstream_blossom_miss_cache: Arc<StdMutex<TimedLruCache<String, ()>>>,
    /// Counters for upstream Blossom read-through decisions.
    pub upstream_blossom_fetch_metrics: Arc<UpstreamBlossomFetchMetrics>,
    /// Write-behind Blossom servers for blobs accepted by this server.
    pub blossom_upload_replicas: Vec<String>,
    /// Background replication queue byte budget. Each queued body holds one
    /// permit per byte until the remote upload attempt finishes.
    pub blossom_upload_replica_queue_bytes: usize,
    pub blossom_upload_replica_queue: Arc<Semaphore>,
    /// Signing key used for server-side write-behind replication auth.
    pub blossom_upload_replica_keys: Option<Arc<Keys>>,
    /// Per-server scheduler that can merge adjacent write-behind replica uploads.
    pub blossom_upload_replica_scheduler:
        Arc<crate::server::blossom::BlossomUploadReplicaScheduler>,
    /// Social graph access control
    pub social_graph: Option<Arc<socialgraph::SocialGraphAccessControl>>,
    /// Social graph store handle for snapshot export
    pub social_graph_store: Option<Arc<dyn socialgraph::SocialGraphBackend>>,
    /// Social graph root pubkey bytes for snapshot export
    pub social_graph_root: Option<[u8; 32]>,
    /// Allow public access to social graph snapshot endpoint
    pub socialgraph_snapshot_public: bool,
    /// Nostr relay state for /ws and WebRTC Nostr messages
    pub nostr_relay: Option<Arc<NostrRelay>>,
    /// Selected provider for Hashtree Nostr root/site lookup and publication.
    pub nostr_provider: Option<Arc<dyn nostr_pubsub::PubsubProvider>>,
    /// Active upstream Nostr relays for HTTP resolver operations.
    pub nostr_relay_urls: Vec<String>,
    /// In-process cache for resolved mutable tree roots, keyed by npub/tree(+key)
    pub tree_root_cache: Arc<StdMutex<HashMap<String, CachedTreeRootEntry>>>,
    /// Shared in-flight blob fetches so concurrent misses only hit upstream once per hash
    pub inflight_blob_fetches: Arc<Mutex<HashMap<String, SharedBlobFetch>>>,
    /// Shared in-flight local blob reads so request bursts for the same hash
    /// only spend one blocking storage read.
    pub inflight_blob_reads: Arc<Mutex<HashMap<String, SharedBlobRead>>>,
    /// Bounded hot cache for immutable blob bodies and metadata probes.
    pub(super) blob_cache: Arc<BlobCache>,
    /// Immutable directory listings keyed by CID
    pub directory_listing_cache: Arc<StdMutex<TimedLruCache<String, LookupResult<Vec<TreeEntry>>>>>,
    /// Immutable resolved paths keyed by root CID + path
    pub resolved_path_cache:
        Arc<StdMutex<TimedLruCache<String, LookupResult<CachedResolvedPathEntry>>>>,
    /// Immutable thumbnail alias resolutions keyed by root CID + alias path
    pub thumbnail_path_cache: Arc<StdMutex<TimedLruCache<String, LookupResult<String>>>>,
    /// Immutable file sizes keyed by CID
    pub cid_size_cache: Arc<StdMutex<TimedLruCache<String, LookupResult<u64>>>>,
}

pub fn new_upstream_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build upstream Blossom HTTP client")
}

#[derive(Clone)]
pub struct AuthCredentials {
    pub username: String,
    pub password: String,
}

fn basic_auth_authorized(headers: &HeaderMap, auth: &AuthCredentials) -> bool {
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let Some(header_value) = auth_header else {
        return false;
    };
    let Some(credentials) = header_value.strip_prefix("Basic ") else {
        return false;
    };

    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;
    let Ok(decoded) = engine.decode(credentials) else {
        return false;
    };
    let Ok(decoded_str) = String::from_utf8(decoded) else {
        return false;
    };
    let expected = format!("{}:{}", auth.username, auth.password);
    decoded_str == expected
}

fn unauthorized_basic_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::WWW_AUTHENTICATE, "Basic realm=\"hashtree\"")
        .body(Body::from("Unauthorized"))
        .unwrap()
}

/// Auth middleware - validates HTTP Basic Auth
pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Result<Response<Body>, StatusCode> {
    // If auth is not enabled, allow request
    let Some(auth) = &state.auth else {
        return Ok(next.run(request).await);
    };

    if basic_auth_authorized(request.headers(), auth) {
        Ok(next.run(request).await)
    } else {
        Ok(unauthorized_basic_response())
    }
}

/// Strict internal auth middleware - requires configured HTTP Basic Auth.
pub async fn require_auth_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Result<Response<Body>, StatusCode> {
    let Some(auth) = &state.auth else {
        return Ok(Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::from("Authentication is not configured"))
            .unwrap());
    };

    if basic_auth_authorized(request.headers(), auth) {
        Ok(next.run(request).await)
    } else {
        Ok(unauthorized_basic_response())
    }
}
