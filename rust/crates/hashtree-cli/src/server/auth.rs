use crate::nostr_relay::NostrRelay;
use crate::socialgraph;
use crate::storage::HashtreeStore;
use crate::webrtc::{PeerRootEvent, WebRTCState};
use axum::{
    body::Body,
    extract::ws::Message,
    extract::State,
    http::{header, Request, Response, StatusCode},
    middleware::Next,
};
use futures::future::{BoxFuture, Shared};
use hashtree_core::{Cid, LinkType, TreeEntry};
use lru::LruCache;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::sync::{
    atomic::{AtomicU32, AtomicU64, Ordering},
    Arc, Mutex as StdMutex,
};
use std::time::{Duration, Instant};
use tokio::{
    sync::{mpsc, watch, Mutex},
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
}

pub type SharedBlobFetch = Shared<BoxFuture<'static, bool>>;

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
    /// WebRTC peer state for forwarding requests to connected P2P peers
    pub webrtc_peers: Option<Arc<WebRTCState>>,
    /// WebSocket relay state for /ws clients
    pub ws_relay: Arc<WsRelayState>,
    /// Maximum upload size in bytes for Blossom uploads (default: 5 MB)
    pub max_upload_bytes: usize,
    /// Allow anyone with valid Nostr auth to write (default: true)
    /// When false, only allowed_pubkeys can write
    pub public_writes: bool,
    /// Pubkeys allowed to write (hex format, from config allowed_npubs)
    pub allowed_pubkeys: HashSet<String>,
    /// Upstream Blossom servers for cascade fetching
    pub upstream_blossom: Vec<String>,
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
    /// Active upstream Nostr relays for HTTP resolver operations.
    pub nostr_relay_urls: Vec<String>,
    /// In-process cache for resolved mutable tree roots, keyed by npub/tree(+key)
    pub tree_root_cache: Arc<StdMutex<HashMap<String, CachedTreeRootEntry>>>,
    /// Shared in-flight blob fetches so concurrent misses only hit upstream once per hash
    pub inflight_blob_fetches: Arc<Mutex<HashMap<String, SharedBlobFetch>>>,
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

#[derive(Clone)]
pub struct AuthCredentials {
    pub username: String,
    pub password: String,
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

    // Check Authorization header
    let auth_header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let authorized = if let Some(header_value) = auth_header {
        if let Some(credentials) = header_value.strip_prefix("Basic ") {
            use base64::Engine;
            let engine = base64::engine::general_purpose::STANDARD;
            if let Ok(decoded) = engine.decode(credentials) {
                if let Ok(decoded_str) = String::from_utf8(decoded) {
                    let expected = format!("{}:{}", auth.username, auth.password);
                    decoded_str == expected
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };

    if authorized {
        Ok(next.run(request).await)
    } else {
        Ok(Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(header::WWW_AUTHENTICATE, "Basic realm=\"hashtree\"")
            .body(Body::from("Unauthorized"))
            .unwrap())
    }
}
