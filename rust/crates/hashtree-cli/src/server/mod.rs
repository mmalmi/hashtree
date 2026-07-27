mod auth;
mod blob_read;
pub mod blossom;
mod handlers;
mod ingest_filter;
mod mime;
mod nostr_query;
mod peer_status;
mod request_paths;
mod status_metrics;
mod ui;
pub mod ws_relay;

use crate::nostr_relay::NostrRelay;
use crate::socialgraph;
use crate::storage::HashtreeStore;
use anyhow::Result;
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderValue, Method, Request, StatusCode},
    middleware,
    middleware::Next,
    response::{IntoResponse, Json, Response},
    routing::{get, post, put},
    Router,
};
use futures::{future::poll_fn, pin_mut, FutureExt};
use hashtree_core::Cid;
use hyper::body::Incoming;
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::conn::auto::Builder as HyperBuilder,
    service::TowerToHyperService,
};
use socket2::{SockRef, TcpKeepalive};
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::future;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tower::{Service, ServiceExt as _};
use tower_http::cors::CorsLayer;
use tracing::{debug, error, trace};

pub use auth::{
    new_lookup_cache, new_upstream_http_client, AppState, AuthCredentials, CachedTreeRootEntry,
};

static VIRTUAL_TREE_HOSTS: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();
const DEFAULT_OPTIMISTIC_UPLOAD_QUEUE_BYTES: usize = 256 * 1024 * 1024;
const DEFAULT_BLOSSOM_UPLOAD_REPLICA_QUEUE_BYTES: usize = 256 * 1024 * 1024;
const INTERNAL_JSON_BODY_LIMIT_BYTES: usize = 64 * 1024;
const POOL_AUDIT_READ_ONLY_HTTP_ERROR: &str = "PoolStore is in audit-serving read-only mode";
const POOL_AUDIT_READ_ONLY_REASON: &str = "pool-audit-read-only";
const POOL_AUDIT_READ_ONLY_REASON_HEADER: &str = "x-hashtree-maintenance-reason";

#[cfg(not(test))]
const HTTP1_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const HTTP1_HEADER_READ_TIMEOUT: Duration = Duration::from_millis(200);
const HTTP2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const HTTP2_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);
const TCP_LISTEN_BACKLOG: i32 = 1_024;
const TCP_KEEPALIVE_TIME: Duration = Duration::from_secs(60);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

pub fn bounded_upload_queue_bytes(bytes: u64) -> usize {
    usize::try_from(bytes)
        .unwrap_or(usize::MAX)
        .clamp(1, tokio::sync::Semaphore::MAX_PERMITS)
}

fn virtual_tree_hosts() -> &'static RwLock<HashMap<String, String>> {
    VIRTUAL_TREE_HOSTS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn pool_audit_request_is_allowed(method: &Method, path: &str) -> bool {
    if path == "/ws" || path == "/ws/" {
        return false;
    }
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
        || (*method == Method::POST && matches!(path, "/blob/batch" | "/upload/check"))
}

async fn pool_audit_read_only_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !state.store.is_pool_audit_read_only()
        || pool_audit_request_is_allowed(request.method(), request.uri().path())
    {
        return next.run(request).await;
    }

    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": POOL_AUDIT_READ_ONLY_HTTP_ERROR,
        })),
    )
        .into_response();
    response.headers_mut().insert(
        header::HeaderName::from_static(POOL_AUDIT_READ_ONLY_REASON_HEADER),
        HeaderValue::from_static(POOL_AUDIT_READ_ONLY_REASON),
    );
    response
}

fn normalize_virtual_tree_host(host: &str) -> Option<String> {
    let trimmed = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(stripped) = trimmed
        .strip_prefix('[')
        .and_then(|value| value.split_once(']'))
    {
        let host_only = stripped.0.trim();
        if host_only.is_empty() {
            return None;
        }
        return Some(host_only.to_string());
    }

    if let Some((host_only, port)) = trimmed.rsplit_once(':') {
        if !host_only.is_empty() && !port.is_empty() && port.chars().all(|ch| ch.is_ascii_digit()) {
            return Some(host_only.to_string());
        }
    }

    Some(trimmed)
}

pub fn register_virtual_tree_host(host: &str, internal_root: &str) {
    let Some(normalized_host) = normalize_virtual_tree_host(host) else {
        return;
    };

    let normalized_root = internal_root.trim().trim_end_matches('/');
    if normalized_root.is_empty() {
        return;
    }

    if let Ok(mut hosts) = virtual_tree_hosts().write() {
        hosts.insert(normalized_host, normalized_root.to_string());
    }
}

pub fn resolve_virtual_tree_host(host: &str) -> Option<String> {
    let normalized_host = normalize_virtual_tree_host(host)?;
    let configured = virtual_tree_hosts()
        .read()
        .ok()
        .and_then(|hosts| hosts.get(&normalized_host).cloned());
    configured.or_else(|| resolve_iris_localhost_tree_root(&normalized_host))
}

fn resolve_iris_localhost_tree_root(host: &str) -> Option<String> {
    let labels: Vec<&str> = host.strip_suffix(".iris.localhost")?.split('.').collect();
    match labels.as_slice() {
        [nhash] if nhash.starts_with("nhash1") => Some(format!("/htree/{nhash}")),
        [site, npub] if !site.is_empty() && npub.starts_with("npub1") => {
            Some(format!("/htree/{npub}/{site}"))
        }
        _ => None,
    }
}

#[cfg(test)]
pub fn clear_virtual_tree_hosts_for_test() {
    if let Ok(mut hosts) = virtual_tree_hosts().write() {
        hosts.clear();
    }
}

pub struct HashtreeServer {
    state: AppState,
    addr: String,
    extra_routes: Option<Router<AppState>>,
    cors: Option<CorsLayer>,
}

impl HashtreeServer {
    pub fn new(store: Arc<HashtreeStore>, addr: String) -> Self {
        Self {
            state: AppState {
                store,
                auth: None,
                daemon_started_at: current_unix_secs(),
                peer_mode: crate::config::ServerMode::Normal,
                hash_get_enabled: true,
                fips_endpoint: None,
                fips_blob_resolver: None,
                fetch_from_fips_peers: true,
                ws_relay: Arc::new(auth::WsRelayState::new()),
                max_upload_bytes: 5 * 1024 * 1024, // 5 MB default
                public_writes: true,               // Allow anyone with valid Nostr auth by default
                public_plaintext_reads: false,
                require_random_untrusted_ingest: true,
                optimistic_blossom_uploads: false,
                optimistic_upload_queue_bytes: DEFAULT_OPTIMISTIC_UPLOAD_QUEUE_BYTES,
                optimistic_upload_queue: Arc::new(tokio::sync::Semaphore::new(
                    DEFAULT_OPTIMISTIC_UPLOAD_QUEUE_BYTES,
                )),
                allowed_pubkeys: HashSet::new(), // No pubkeys allowed by default (use public_writes)
                upstream_blossom: Vec::new(),
                upstream_http_client: new_upstream_http_client(),
                upstream_blossom_miss_cache: Arc::new(std::sync::Mutex::new(new_lookup_cache())),
                upstream_blossom_fetch_metrics: Arc::new(
                    auth::UpstreamBlossomFetchMetrics::default(),
                ),
                blossom_upload_replicas: Vec::new(),
                blossom_upload_replica_queue_bytes: DEFAULT_BLOSSOM_UPLOAD_REPLICA_QUEUE_BYTES,
                blossom_upload_replica_queue: Arc::new(tokio::sync::Semaphore::new(
                    DEFAULT_BLOSSOM_UPLOAD_REPLICA_QUEUE_BYTES,
                )),
                blossom_upload_replica_keys: None,
                blossom_upload_replica_scheduler: Arc::new(
                    blossom::BlossomUploadReplicaScheduler::new(),
                ),
                social_graph: None,
                social_graph_store: None,
                social_graph_root: None,
                socialgraph_snapshot_public: false,
                nostr_relay: None,
                nostr_provider: None,
                nostr_relay_urls: Vec::new(),
                tree_root_cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
                inflight_blob_fetches: Arc::new(tokio::sync::Mutex::new(
                    std::collections::HashMap::new(),
                )),
                inflight_blob_reads: Arc::new(tokio::sync::Mutex::new(
                    std::collections::HashMap::new(),
                )),
                blob_cache: Arc::new(crate::blob_cache::BlobCache::from_env()),
                directory_listing_cache: Arc::new(std::sync::Mutex::new(new_lookup_cache())),
                resolved_path_cache: Arc::new(std::sync::Mutex::new(new_lookup_cache())),
                thumbnail_path_cache: Arc::new(std::sync::Mutex::new(new_lookup_cache())),
                cid_size_cache: Arc::new(std::sync::Mutex::new(new_lookup_cache())),
            },
            addr,
            extra_routes: None,
            cors: None,
        }
    }

    /// Set maximum upload size for Blossom uploads
    pub fn with_max_upload_bytes(mut self, bytes: usize) -> Self {
        self.state.max_upload_bytes = bytes;
        self
    }

    /// Set whether to allow public writes (anyone with valid Nostr auth)
    /// When false, only social graph members can write
    pub fn with_public_writes(mut self, public: bool) -> Self {
        self.state.public_writes = public;
        self
    }

    /// Set whether mutable npub routes serve plaintext for unapproved pubkeys
    pub fn with_public_plaintext_reads(mut self, public: bool) -> Self {
        self.state.public_plaintext_reads = public;
        self
    }

    pub fn with_require_random_untrusted_ingest(mut self, require: bool) -> Self {
        self.state.require_random_untrusted_ingest = require;
        self
    }

    pub fn with_optimistic_blossom_uploads(mut self, enabled: bool) -> Self {
        self.state.optimistic_blossom_uploads = enabled;
        self
    }

    pub fn with_server_mode(mut self, mode: crate::config::ServerMode) -> Self {
        self.state.peer_mode = mode;
        self
    }

    pub fn with_hash_get_enabled(mut self, enabled: bool) -> Self {
        self.state.hash_get_enabled = enabled;
        self
    }

    pub fn with_fetch_from_fips_peers(mut self, enabled: bool) -> Self {
        self.state.fetch_from_fips_peers = enabled;
        self
    }

    pub fn with_fips_endpoint(
        mut self,
        endpoint: Arc<hashtree_fips_transport::FipsEndpoint>,
    ) -> Self {
        self.state.fips_endpoint = Some(endpoint);
        self
    }

    pub fn with_fips_blob_resolver(
        mut self,
        resolver: Arc<crate::fips_transport::DaemonBlobResolver>,
    ) -> Self {
        self.state.fips_blob_resolver = Some(resolver);
        self
    }

    pub fn with_auth(mut self, username: String, password: String) -> Self {
        self.state.auth = Some(AuthCredentials { username, password });
        self
    }

    /// Set allowed pubkeys for blossom write access (hex format)
    pub fn with_allowed_pubkeys(mut self, pubkeys: HashSet<String>) -> Self {
        self.state.allowed_pubkeys = pubkeys;
        self
    }

    /// Set upstream Blossom servers for cascade fetching
    pub fn with_upstream_blossom(mut self, servers: Vec<String>) -> Self {
        self.state.upstream_blossom = servers;
        self
    }

    /// Set write-behind Blossom servers for blobs accepted by this server.
    pub fn with_blossom_upload_replicas(
        mut self,
        servers: Vec<String>,
        queue_bytes: usize,
        keys: nostr::Keys,
    ) -> Self {
        let queue_bytes = queue_bytes.clamp(1, tokio::sync::Semaphore::MAX_PERMITS);
        let mut servers: Vec<String> = servers
            .into_iter()
            .map(|server| server.trim().trim_end_matches('/').to_string())
            .filter(|server| !server.is_empty())
            .collect();
        servers.sort();
        servers.dedup();
        let replica_keys = (!servers.is_empty()).then(|| Arc::new(keys));
        self.state.blossom_upload_replicas = servers;
        self.state.blossom_upload_replica_queue_bytes = queue_bytes;
        self.state.blossom_upload_replica_queue =
            Arc::new(tokio::sync::Semaphore::new(queue_bytes));
        self.state.blossom_upload_replica_keys = replica_keys;
        self
    }

    /// Set social graph access control
    pub fn with_social_graph(mut self, sg: Arc<socialgraph::SocialGraphAccessControl>) -> Self {
        self.state.social_graph = Some(sg);
        self
    }

    /// Configure social graph snapshot export (store handle + root)
    pub fn with_socialgraph_snapshot(
        mut self,
        store: Arc<dyn socialgraph::SocialGraphBackend>,
        root: [u8; 32],
        public: bool,
    ) -> Self {
        self.state.social_graph_store = Some(store);
        self.state.social_graph_root = Some(root);
        self.state.socialgraph_snapshot_public = public;
        self
    }

    /// Set Nostr relay state (shared for /ws and WebRTC)
    pub fn with_nostr_relay(mut self, relay: Arc<NostrRelay>) -> Self {
        self.state.nostr_relay = Some(relay);
        self
    }

    pub fn with_nostr_provider(mut self, provider: Arc<dyn nostr_pubsub::PubsubProvider>) -> Self {
        self.state.nostr_provider = Some(provider);
        self
    }

    /// Set active upstream Nostr relays for HTTP resolver operations.
    pub fn with_nostr_relay_urls(mut self, relays: Vec<String>) -> Self {
        self.state.nostr_relay_urls = relays;
        self
    }

    /// Seed mutable root cache entries before the server starts.
    pub fn with_cached_tree_roots(self, roots: Vec<(String, Cid)>) -> Self {
        if let Ok(mut cache) = self.state.tree_root_cache.lock() {
            let now = Instant::now();
            for (key, cid) in roots {
                cache.insert(
                    key,
                    CachedTreeRootEntry {
                        cid,
                        source: "embedded-bootstrap",
                        root_event: None,
                        event: None,
                        cached_at: now,
                    },
                );
            }
        }
        self
    }

    /// Merge extra routes into the daemon router (e.g. Tauri embeds /nip07).
    pub fn with_extra_routes(mut self, routes: Router<AppState>) -> Self {
        self.extra_routes = Some(routes);
        self
    }

    /// Apply a CORS layer to all routes (used by embedded clients like Tauri).
    pub fn with_cors(mut self, cors: CorsLayer) -> Self {
        self.cors = Some(cors);
        self
    }

    pub async fn run(self) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(&self.addr).await?;
        let _ = self.run_with_listener(listener).await?;
        Ok(())
    }

    pub async fn run_with_listener(self, listener: tokio::net::TcpListener) -> Result<u16> {
        self.run_with_listener_until(listener, future::pending::<()>())
            .await
    }

    pub async fn run_with_listener_until<F>(
        self,
        listener: tokio::net::TcpListener,
        shutdown: F,
    ) -> Result<u16>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        // `tokio::net::TcpListener::bind` inherits the platform's small
        // default listen backlog (128 on Linux). A daemon restart can attract
        // more reconnecting CDN/tunnel sockets than that before the first
        // requests are accepted, leaving otherwise healthy clients stalled.
        // Re-listening updates the queue limit without replacing the bound
        // socket; the kernel still applies its configured `somaxconn` cap.
        SockRef::from(&listener).listen(TCP_LISTEN_BACKLOG)?;
        let local_addr = listener.local_addr()?;

        // Public endpoints (no auth required)
        // Note: /:id serves raw SHA256 blobs only. Logical tree/file assembly
        // stays on explicitly configured mutable routes such as approved npub.
        let state = self.state.clone();
        let public_routes = Router::new()
            .route("/", get(handlers::serve_root_or_virtual_host))
            .route("/ws", get(ws_relay::ws_data))
            .route("/ws/", get(ws_relay::ws_data))
            .route(
                "/__iris/store/:hash",
                get(handlers::iris_store_get).head(handlers::iris_store_head),
            )
            .route(
                "/htree/test",
                get(handlers::htree_test).head(handlers::htree_test),
            )
            // /htree/nhash1...[/path] - content-addressed (immutable)
            .route("/htree/nhash1:nhash", get(handlers::htree_nhash))
            .route("/htree/nhash1:nhash/", get(handlers::htree_nhash))
            .route("/htree/nhash1:nhash/*path", get(handlers::htree_nhash_path))
            // /htree/npub1.../tree[/path] - mutable (resolver-backed)
            .route("/htree/npub1:npub/:treename", get(handlers::htree_npub))
            .route("/htree/npub1:npub/:treename/", get(handlers::htree_npub))
            .route(
                "/htree/npub1:npub/:treename/*path",
                get(handlers::htree_npub_path),
            )
            // Nostr resolver endpoints - resolve npub/treename to content
            .route("/n/:pubkey/:treename", get(handlers::resolve_and_serve))
            // Direct npub route (clients should parse nhash and request by hex hash)
            .route("/npub1:rest", get(handlers::serve_npub))
            .route("/npub1:rest/*path", get(handlers::serve_npub))
            // Blossom endpoints (BUD-01, BUD-02)
            .route(
                "/:id",
                get(handlers::serve_content_or_blob)
                    .head(blossom::head_blob)
                    .delete(blossom::delete_blob)
                    .options(blossom::cors_preflight),
            )
            .route(
                "/upload",
                put(blossom::upload_blob)
                    .layer::<_, std::convert::Infallible>(middleware::from_fn(
                        blossom::require_upload_auth_middleware,
                    ))
                    .layer(DefaultBodyLimit::max(blossom::MAX_SINGLE_UPLOAD_BODY_BYTES))
                    .head(blossom::head_upload)
                    .options(blossom::cors_preflight),
            )
            .route(
                "/upload/batch",
                post(blossom::upload_blob_batch)
                    .layer::<_, std::convert::Infallible>(middleware::from_fn(
                        blossom::require_upload_auth_middleware,
                    ))
                    .options(blossom::cors_preflight)
                    .layer(DefaultBodyLimit::max(
                        blossom::MAX_BATCH_UPLOAD_JSON_BODY_BYTES,
                    )),
            )
            .route(
                "/upload/batch-binary",
                post(blossom::upload_blob_batch_binary)
                    .layer::<_, std::convert::Infallible>(middleware::from_fn(
                        blossom::require_upload_auth_middleware,
                    ))
                    .options(blossom::cors_preflight)
                    .layer(DefaultBodyLimit::max(
                        blossom::MAX_BATCH_UPLOAD_BINARY_BODY_BYTES,
                    )),
            )
            .route(
                "/upload/check",
                post(blossom::upload_check).options(blossom::cors_preflight),
            )
            .route(
                "/blob/batch",
                post(handlers::download_blob_batch).options(blossom::cors_preflight),
            )
            .route(
                "/list/:pubkey",
                get(blossom::list_blobs).options(blossom::cors_preflight),
            )
            // Hashtree API endpoints
            .route("/health", get(handlers::health_check))
            .route("/api/pins", get(handlers::list_pins))
            .route("/api/stats", get(handlers::storage_stats))
            .route("/api/status", get(handlers::daemon_status))
            .route("/api/socialgraph", get(handlers::socialgraph_stats))
            .route(
                "/api/socialgraph/snapshot",
                get(handlers::socialgraph_snapshot),
            )
            .route(
                "/api/socialgraph/distance/:pubkey",
                get(handlers::follow_distance),
            )
            // Resolver API endpoints
            .route(
                "/api/resolve/:pubkey/:treename",
                get(handlers::resolve_to_hash),
            )
            .route(
                "/api/nostr/resolve/:pubkey/:treename",
                get(handlers::resolve_to_hash),
            )
            .route("/api/nostr/profile/:pubkey", get(handlers::nostr_profile))
            .route(
                "/api/nostr/events",
                post(handlers::publish_nostr_event)
                    .layer(DefaultBodyLimit::max(INTERNAL_JSON_BODY_LIMIT_BYTES)),
            )
            .route("/api/trees/:pubkey", get(handlers::list_trees))
            .fallback(get(handlers::serve_virtual_host_fallback))
            .with_state(state.clone());

        // Protected endpoints (require auth if enabled)
        let protected_routes = Router::new()
            .route("/upload", post(handlers::upload_file))
            .route("/api/pin/:cid", post(handlers::pin_cid))
            .route("/api/unpin/:cid", post(handlers::unpin_cid))
            .route("/api/gc", post(handlers::garbage_collect))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth::auth_middleware,
            ))
            .with_state(state.clone());

        // Internal mutating endpoints require configured Basic auth. These
        // routes stay closed even when optional API auth is disabled.
        let internal_routes = Router::new()
            .route(
                "/__iris/store/:hash",
                put(handlers::iris_store_put)
                    .delete(handlers::iris_store_delete)
                    .layer(DefaultBodyLimit::max(blossom::MAX_BATCH_UPLOAD_BYTES)),
            )
            .route(
                "/api/pin-tree",
                post(handlers::pin_tree)
                    .layer(DefaultBodyLimit::max(INTERNAL_JSON_BODY_LIMIT_BYTES)),
            )
            .route(
                "/api/cache-tree-root",
                post(handlers::cache_tree_root)
                    .layer(DefaultBodyLimit::max(INTERNAL_JSON_BODY_LIMIT_BYTES)),
            )
            .route(
                "/api/clear-tree-root-cache",
                post(handlers::clear_tree_root_cache)
                    .layer(DefaultBodyLimit::max(INTERNAL_JSON_BODY_LIMIT_BYTES)),
            )
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth::require_auth_middleware,
            ))
            .with_state(state.clone());

        let mut app = public_routes
            .merge(protected_routes)
            .merge(internal_routes)
            .layer(DefaultBodyLimit::max(10 * 1024 * 1024 * 1024)); // 10GB limit

        if let Some(extra) = self.extra_routes {
            app = app.merge(extra.with_state(state.clone()));
        }

        // This gate is deliberately outside every route-level auth/body
        // layer, including caller-supplied routes. Audit mode rejects
        // mutation ingress before any handler can alter blob or metadata
        // state. Status metrics remain outermost so maintenance responses are
        // still observable.
        app = app
            .layer(middleware::from_fn_with_state(
                state,
                pool_audit_read_only_middleware,
            ))
            .layer(middleware::from_fn(status_metrics::record_http_status));

        if let Some(cors) = self.cors {
            app = app.layer(cors);
        }

        let make_service = app.into_make_service_with_connect_info::<std::net::SocketAddr>();
        serve_with_connection_limits(listener, make_service, shutdown).await?;

        Ok(local_addr.port())
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }
}

async fn serve_with_connection_limits<M, S, F>(
    listener: tokio::net::TcpListener,
    mut make_service: M,
    shutdown: F,
) -> io::Result<()>
where
    M: Service<SocketAddr, Error = Infallible, Response = S> + Send + 'static,
    M::Future: Send,
    S: Service<Request<Body>, Response = Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send,
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let (signal_tx, signal_rx) = watch::channel(());
    let signal_tx = Arc::new(signal_tx);
    tokio::spawn(async move {
        shutdown.await;
        trace!("received graceful shutdown signal; stopping daemon listener");
        drop(signal_rx);
    });

    let (close_tx, close_rx) = watch::channel(());

    loop {
        let (tcp_stream, remote_addr) = tokio::select! {
            accepted = accept_tcp(&listener) => {
                match accepted? {
                    Some(connection) => connection,
                    None => continue,
                }
            }
            _ = signal_tx.closed() => {
                trace!("shutdown signal received; no longer accepting daemon connections");
                break;
            }
        };

        configure_tcp_stream(&tcp_stream);
        let tcp_stream = TokioIo::new(tcp_stream);

        poll_fn(|cx| make_service.poll_ready(cx))
            .await
            .unwrap_or_else(|err| match err {});

        let tower_service = make_service
            .call(remote_addr)
            .await
            .unwrap_or_else(|err| match err {})
            .map_request(|req: Request<Incoming>| req.map(Body::new));
        let hyper_service = TowerToHyperService::new(tower_service);

        let signal_tx = Arc::clone(&signal_tx);
        let close_rx = close_rx.clone();

        tokio::spawn(async move {
            let mut builder = HyperBuilder::new(TokioExecutor::new());
            builder
                .http1()
                .timer(TokioTimer::new())
                .header_read_timeout(HTTP1_HEADER_READ_TIMEOUT);
            builder
                .http2()
                .timer(TokioTimer::new())
                .keep_alive_interval(Some(HTTP2_KEEPALIVE_INTERVAL))
                .keep_alive_timeout(HTTP2_KEEPALIVE_TIMEOUT);

            let conn = builder.serve_connection_with_upgrades(tcp_stream, hyper_service);
            pin_mut!(conn);

            let signal_closed = signal_tx.closed().fuse();
            pin_mut!(signal_closed);

            loop {
                tokio::select! {
                    result = conn.as_mut() => {
                        if let Err(err) = result {
                            trace!("daemon connection closed with error: {err:#}");
                        }
                        break;
                    }
                    _ = &mut signal_closed => {
                        trace!("shutdown signal received by connection task");
                        conn.as_mut().graceful_shutdown();
                    }
                }
            }

            drop(close_rx);
        });
    }

    drop(close_rx);
    drop(listener);
    close_tx.closed().await;

    Ok(())
}

fn configure_tcp_stream(tcp_stream: &tokio::net::TcpStream) {
    if let Err(err) = tcp_stream.set_nodelay(true) {
        debug!("failed to set TCP_NODELAY on daemon connection: {err:#}");
    }

    let socket = SockRef::from(tcp_stream);
    if let Err(err) = socket.set_tcp_keepalive(
        &TcpKeepalive::new()
            .with_time(TCP_KEEPALIVE_TIME)
            .with_interval(TCP_KEEPALIVE_INTERVAL),
    ) {
        debug!("failed to set TCP keepalive on daemon connection: {err:#}");
    }
}

async fn accept_tcp(
    listener: &tokio::net::TcpListener,
) -> io::Result<Option<(tokio::net::TcpStream, SocketAddr)>> {
    match listener.accept().await {
        Ok(connection) => Ok(Some(connection)),
        Err(err) => {
            if is_connection_error(&err) {
                return Ok(None);
            }
            if is_resource_exhaustion_error(&err) {
                error!(
                    "daemon accept failed due to file descriptor exhaustion; exiting for supervisor restart: {err}"
                );
                return Err(err);
            }
            error!("daemon accept error: {err}");
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok(None)
        }
    }
}

fn is_resource_exhaustion_error(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(code) if code == libc::EMFILE || code == libc::ENFILE
    )
}

fn is_connection_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

fn current_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO)
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nostr_relay::{NostrRelay, NostrRelayConfig};
    use crate::storage::HashtreeStore;
    use async_trait::async_trait;
    use hashtree_config::StorageBackend;
    use hashtree_core::types::Hash;
    use hashtree_core::{
        from_hex, nhash_encode, nhash_encode_full, sha256, to_hex, DirEntry, HashTree,
        HashTreeConfig, LinkType, NHashData,
    };
    use nostr::{nips::nip19::ToBech32, EventBuilder, Keys, Kind, Timestamp};
    use nostr_pubsub::{
        EventBus, EventSource, PublishReport, PubsubProvider, PubsubProviderMode, QueryEvent,
        QueryOptions, QueryReport, VerifiedEvent,
    };
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use walkdir::WalkDir;

    const AUDIT_SERVER_CHILD_DATA_ENV: &str = "HASHTREE_AUDIT_SERVER_CHILD_DATA";
    const AUDIT_SERVER_CHILD_HASH_ENV: &str = "HASHTREE_AUDIT_SERVER_CHILD_HASH";

    fn pool_data_file_snapshot(root: &Path) -> Vec<(PathBuf, Hash, std::time::SystemTime)> {
        let mut snapshot = WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| {
                if !entry.file_type().is_file() || entry.file_name() != "data.mdb" {
                    return false;
                }
                matches!(
                    entry
                        .path()
                        .parent()
                        .and_then(Path::file_name)
                        .and_then(|name| name.to_str()),
                    Some(hashtree_lmdb::SHARED_BLOB_POOL_DIR_NAME) | Some("blobs")
                )
            })
            .map(|entry| {
                let path = entry.into_path();
                let bytes = std::fs::read(&path).expect("read Pool LMDB data file");
                let modified = std::fs::metadata(&path)
                    .expect("read Pool LMDB metadata")
                    .modified()
                    .expect("read Pool LMDB mtime");
                (path, sha256(&bytes), modified)
            })
            .collect::<Vec<_>>();
        snapshot.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        snapshot
    }

    async fn assert_pool_audit_maintenance_response(response: reqwest::Response) -> Result<()> {
        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response
                .headers()
                .get(POOL_AUDIT_READ_ONLY_REASON_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(POOL_AUDIT_READ_ONLY_REASON)
        );
        let body: serde_json::Value = response.json().await?;
        assert_eq!(
            body,
            json!({
                "error": POOL_AUDIT_READ_ONLY_HTTP_ERROR,
            })
        );
        Ok(())
    }

    struct StaticProvider {
        event: Option<VerifiedEvent>,
        queries: AtomicUsize,
        publishes: AtomicUsize,
        mode: PubsubProviderMode,
    }

    #[test]
    fn pool_audit_request_gate_only_allows_read_ingress() {
        for method in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(pool_audit_request_is_allowed(&method, "/health"));
        }
        assert!(pool_audit_request_is_allowed(&Method::POST, "/blob/batch"));
        assert!(pool_audit_request_is_allowed(
            &Method::POST,
            "/upload/check"
        ));
        for (method, path) in [
            (Method::GET, "/ws"),
            (Method::GET, "/ws/"),
            (Method::PUT, "/upload"),
            (Method::POST, "/upload/batch"),
            (Method::POST, "/upload/batch-binary"),
            (Method::POST, "/api/pin/hash"),
            (Method::POST, "/api/nostr/events"),
            (Method::DELETE, "/hash"),
        ] {
            assert!(
                !pool_audit_request_is_allowed(&method, path),
                "{method} {path} must be blocked"
            );
        }
    }

    #[tokio::test]
    #[ignore = "subprocess entry point for Pool audit-serving HTTP verification"]
    async fn pool_audit_read_only_server_subprocess() -> Result<()> {
        let Some(data_dir) = std::env::var_os(AUDIT_SERVER_CHILD_DATA_ENV) else {
            return Ok(());
        };
        let hash_hex = std::env::var(AUDIT_SERVER_CHILD_HASH_ENV)?;
        let store = Arc::new(HashtreeStore::new_with_backend(
            data_dir,
            StorageBackend::Lmdb,
            16 * 1024 * 1024 * 1024,
        )?);
        assert!(store.is_pool_audit_read_only());

        let owner = Keys::generate();
        let tree_name = "pool-audit-external";
        let root_event = EventBuilder::new(Kind::Custom(30064), "")
            .tags(vec![
                nostr::Tag::identifier(tree_name),
                nostr::Tag::custom(nostr::TagKind::custom("l"), vec!["hashtree".to_string()]),
                nostr::Tag::custom(nostr::TagKind::custom("hash"), vec![hash_hex.clone()]),
            ])
            .sign_with_keys(&owner)?;
        let provider = Arc::new(StaticProvider {
            event: Some(VerifiedEvent::try_from(root_event.clone())?),
            queries: AtomicUsize::new(0),
            publishes: AtomicUsize::new(0),
            mode: PubsubProviderMode::LocalOnly,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let server = HashtreeServer::new(Arc::clone(&store), "127.0.0.1:0".to_string())
            .with_nostr_provider(provider.clone());
        let handle =
            tokio::spawn(async move { server.run_with_listener(listener).await.map(|_| ()) });
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");

        let response = client.get(format!("{base}/{hash_hex}")).send().await?;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.bytes().await?.as_ref(), b"audit-serving bytes");

        let status: serde_json::Value = client
            .get(format!("{base}/api/status"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(status["pool_audit_read_only"], true);
        assert_eq!(status["capabilities"]["writes"], false);

        let npub = owner.public_key().to_bech32()?;
        let resolved: serde_json::Value = client
            .get(format!(
                "{base}/api/nostr/resolve/{npub}/{tree_name}?refresh=1"
            ))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(resolved["hash"], hash_hex);
        assert_eq!(resolved["source"], "fips-pubsub");
        assert_eq!(resolved["event_id"], root_event.id.to_hex());
        assert_eq!(provider.queries.load(Ordering::Relaxed), 1);
        assert_eq!(provider.publishes.load(Ordering::Relaxed), 0);

        assert_pool_audit_maintenance_response(
            client
                .post(format!("{base}/api/nostr/events"))
                .json(&root_event)
                .send()
                .await?,
        )
        .await?;
        assert_eq!(provider.publishes.load(Ordering::Relaxed), 0);

        assert_pool_audit_maintenance_response(
            client
                .put(format!("{base}/upload"))
                .body(b"blocked upload".to_vec())
                .send()
                .await?,
        )
        .await?;
        assert_pool_audit_maintenance_response(
            client
                .post(format!("{base}/upload/batch"))
                .json(&json!({"blobs": []}))
                .send()
                .await?,
        )
        .await?;
        assert_pool_audit_maintenance_response(
            client.delete(format!("{base}/{hash_hex}")).send().await?,
        )
        .await?;
        assert_pool_audit_maintenance_response(client.get(format!("{base}/ws")).send().await?)
            .await?;

        let response = client.get(format!("{base}/{hash_hex}")).send().await?;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.bytes().await?.as_ref(), b"audit-serving bytes");

        handle.abort();
        Ok(())
    }

    #[test]
    fn pool_audit_read_only_server_resolves_external_nostr_without_mutation() -> Result<()> {
        let temp = TempDir::new()?;
        let data_dir = temp.path().join("data");
        let store = HashtreeStore::new_with_backend(
            &data_dir,
            StorageBackend::Lmdb,
            16 * 1024 * 1024 * 1024,
        )?;
        let hash_hex = store.put_blob(b"audit-serving bytes")?;
        store.force_sync()?;
        drop(store);

        let before = pool_data_file_snapshot(&data_dir);
        assert!(
            before.len() >= 2,
            "expected Pool catalog and at least one member data file"
        );

        let output = Command::new(std::env::current_exe()?)
            .arg("--ignored")
            .arg("--exact")
            .arg("server::tests::pool_audit_read_only_server_subprocess")
            .arg("--nocapture")
            .env(AUDIT_SERVER_CHILD_DATA_ENV, &data_dir)
            .env(AUDIT_SERVER_CHILD_HASH_ENV, &hash_hex)
            .env(hashtree_lmdb::POOL_AUDIT_READ_ONLY_ENV, "1")
            .env_remove("HTREE_LMDB_HOT_BLOB_DIR")
            .env_remove("HTREE_LMDB_HOT_BLOB_LEGACY_DIR")
            .env_remove("HTREE_LMDB_HOT_EXTERNAL_BLOB_DIR")
            .env_remove("HTREE_LMDB_LEGACY_EXTERNAL_BLOB_DIR")
            .env("RUST_TEST_THREADS", "1")
            .output()?;
        assert!(
            output.status.success(),
            "audit-serving subprocess failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let after = pool_data_file_snapshot(&data_dir);
        assert_eq!(
            after, before,
            "audit-serving process mutated Pool data files"
        );
        Ok(())
    }

    #[async_trait]
    impl EventBus for StaticProvider {
        async fn publish(
            &self,
            _event: VerifiedEvent,
            _source: EventSource,
        ) -> nostr_pubsub::Result<PublishReport> {
            self.publishes.fetch_add(1, Ordering::Relaxed);
            Ok(PublishReport {
                accepted: true,
                priority: 0,
                reason: None,
            })
        }

        async fn query(
            &self,
            _filters: Vec<nostr_pubsub::Filter>,
            _options: QueryOptions,
        ) -> nostr_pubsub::Result<QueryReport> {
            self.queries.fetch_add(1, Ordering::Relaxed);
            Ok(QueryReport {
                events: self
                    .event
                    .clone()
                    .map(|event| QueryEvent {
                        event,
                        source: EventSource::fips_endpoint("browser-router"),
                        priority: 0,
                    })
                    .into_iter()
                    .collect(),
            })
        }
    }

    impl PubsubProvider for StaticProvider {
        fn mode(&self) -> PubsubProviderMode {
            self.mode
        }
    }

    #[test]
    fn resource_exhaustion_errors_are_fatal_accept_errors() {
        assert!(is_resource_exhaustion_error(&io::Error::from_raw_os_error(
            libc::EMFILE
        )));
        assert!(is_resource_exhaustion_error(&io::Error::from_raw_os_error(
            libc::ENFILE
        )));
        assert!(!is_resource_exhaustion_error(
            &io::Error::from_raw_os_error(libc::ECONNRESET)
        ));
    }

    #[test]
    fn upload_queue_semaphores_fit_all_tokio_targets() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);
        let server = HashtreeServer::new(store, "127.0.0.1:0".to_string());

        assert_eq!(
            server.state.optimistic_upload_queue.available_permits(),
            256 * 1024 * 1024
        );
        assert_eq!(
            server
                .state
                .blossom_upload_replica_queue
                .available_permits(),
            256 * 1024 * 1024
        );
        assert_eq!(
            bounded_upload_queue_bytes(u64::MAX),
            tokio::sync::Semaphore::MAX_PERMITS
        );
        Ok(())
    }

    #[test]
    fn server_builder_seeds_initial_tree_roots() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);
        let hash = from_hex("1111111111111111111111111111111111111111111111111111111111111111")?;
        let key = from_hex("2222222222222222222222222222222222222222222222222222222222222222")?;
        let cid = Cid {
            hash,
            key: Some(key),
        };

        let server = HashtreeServer::new(store, "127.0.0.1:0".to_string())
            .with_cached_tree_roots(vec![("npub1example/sites".to_string(), cid.clone())]);
        let cached = server
            .state
            .tree_root_cache
            .lock()
            .unwrap()
            .get("npub1example/sites")
            .cloned()
            .expect("seeded root");

        assert_eq!(cached.cid, cid);
        assert_eq!(cached.source, "embedded-bootstrap");
        assert!(cached.root_event.is_none());
        Ok(())
    }

    #[test]
    fn iris_localhost_hosts_map_to_existing_tree_routes() {
        assert_eq!(
            resolve_virtual_tree_host("NHASH1EXAMPLE.iris.localhost:8080"),
            Some("/htree/nhash1example".to_string())
        );
        assert_eq!(
            resolve_virtual_tree_host("audio.NPUB1EXAMPLE.iris.localhost:8080"),
            Some("/htree/npub1example/audio".to_string())
        );
        assert_eq!(
            resolve_virtual_tree_host("audio.extra.npub1example.iris.localhost"),
            None
        );
        assert_eq!(
            resolve_virtual_tree_host("nhash1example.htree.localhost"),
            None
        );
    }

    #[tokio::test]
    async fn test_server_serve_file() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);

        // Create and upload a test file
        let test_file = temp_dir.path().join("test.txt");
        std::fs::write(&test_file, b"Hello, Hashtree!")?;

        let cid = store.upload_file(&test_file)?;
        let hash = from_hex(&cid)?;

        // Verify we can get it
        let content = store.get_file(&hash)?;
        assert!(content.is_some());
        assert_eq!(content.unwrap(), b"Hello, Hashtree!");

        Ok(())
    }

    #[tokio::test]
    async fn test_server_list_pins() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);

        let test_file = temp_dir.path().join("test.txt");
        std::fs::write(&test_file, b"Test")?;

        let cid = store.upload_file(&test_file)?;
        let hash = from_hex(&cid)?;

        let pins = store.list_pins_raw()?;
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0], hash);

        Ok(())
    }

    async fn spawn_test_server(
        store: Arc<HashtreeStore>,
    ) -> Result<(u16, tokio::task::JoinHandle<Result<()>>)> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let server = HashtreeServer::new(store, "127.0.0.1:0".to_string());
        let handle =
            tokio::spawn(async move { server.run_with_listener(listener).await.map(|_| ()) });
        Ok((port, handle))
    }

    async fn spawn_test_server_with_auth(
        store: Arc<HashtreeStore>,
    ) -> Result<(u16, tokio::task::JoinHandle<Result<()>>)> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let server = HashtreeServer::new(store, "127.0.0.1:0".to_string())
            .with_auth("test-user".to_string(), "test-password".to_string());
        let handle =
            tokio::spawn(async move { server.run_with_listener(listener).await.map(|_| ()) });
        Ok((port, handle))
    }

    async fn encrypted_test_directory(store: &HashtreeStore) -> Result<(Cid, Cid, Vec<u8>)> {
        let tree = HashTree::new(
            HashTreeConfig::new(store.store_arc())
                .with_chunk_size(4)
                .with_max_links(2),
        );
        let content = b"encrypted descendant content spanning chunks".to_vec();
        let (file_cid, file_size) = tree.put(&content).await?;
        let nested_cid = tree
            .put_directory(vec![DirEntry::from_cid("post.json", &file_cid)
                .with_size(file_size)
                .with_link_type(LinkType::File)])
            .await?;
        let root_cid = tree
            .put_directory(vec![
                DirEntry::from_cid("events", &nested_cid).with_link_type(LinkType::Dir)
            ])
            .await?;
        Ok((root_cid, file_cid, content))
    }

    async fn spawn_test_server_with_nostr_relay(
        store: Arc<HashtreeStore>,
        relay: Arc<NostrRelay>,
    ) -> Result<(u16, tokio::task::JoinHandle<Result<()>>)> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let server = HashtreeServer::new(store, "127.0.0.1:0".to_string()).with_nostr_relay(relay);
        let handle =
            tokio::spawn(async move { server.run_with_listener(listener).await.map(|_| ()) });
        Ok((port, handle))
    }

    #[tokio::test]
    async fn unauthenticated_native_store_mutation_is_rejected() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);
        let body = b"unauthorized native store write";
        let hash = sha256(body);
        let hash_hex = to_hex(&hash);
        let (port, handle) = spawn_test_server(Arc::clone(&store)).await?;

        let response = reqwest::Client::new()
            .put(format!("http://127.0.0.1:{port}/__iris/store/{hash_hex}"))
            .body(body.to_vec())
            .send()
            .await?;

        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
        assert!(store.get_blob(&hash)?.is_none());
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn unauthenticated_cache_tree_root_is_rejected() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);
        let (port, handle) = spawn_test_server(store).await?;

        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/api/cache-tree-root"))
            .json(&json!({
                "npub": "npub1example",
                "treeName": "video",
                "hash": "988db3f24dc222715f1c1e1fa5876690d3147122243d72d85fd44283867cd61a",
                "visibility": "public"
            }))
            .send()
            .await?;

        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn pin_tree_requires_configured_valid_auth() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);
        let nhash = nhash_encode(&sha256(b"missing"))?;
        let client = reqwest::Client::new();

        let (port, handle) = spawn_test_server(Arc::clone(&store)).await?;
        let response = client
            .post(format!("http://127.0.0.1:{port}/api/pin-tree"))
            .json(&json!({"nhash": nhash}))
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
        handle.abort();

        let (port, handle) = spawn_test_server_with_auth(store).await?;
        let response = client
            .post(format!("http://127.0.0.1:{port}/api/pin-tree"))
            .json(&json!({"nhash": nhash}))
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn pin_tree_rejects_malformed_noncanonical_and_missing_roots() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);
        let missing_hash = sha256(b"not stored");
        let canonical = nhash_encode(&missing_hash)?;
        let (port, handle) = spawn_test_server_with_auth(Arc::clone(&store)).await?;
        let client = reqwest::Client::new();

        for nhash in ["not-an-nhash".to_string(), format!("hashtree:{canonical}")] {
            let response = client
                .post(format!("http://127.0.0.1:{port}/api/pin-tree"))
                .basic_auth("test-user", Some("test-password"))
                .json(&json!({"nhash": nhash}))
                .send()
                .await?;
            assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        }

        let response = client
            .post(format!("http://127.0.0.1:{port}/api/pin-tree"))
            .basic_auth("test-user", Some("test-password"))
            .json(&json!({"nhash": canonical}))
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        assert!(!store.is_pinned(&missing_hash)?);
        assert!(store.get_tree_meta(&missing_hash)?.is_none());
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn pin_tree_indexes_encrypted_descendants_and_is_idempotent() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);
        let (root_cid, file_cid, expected_content) = encrypted_test_directory(&store).await?;
        let nhash = nhash_encode_full(&NHashData {
            hash: root_cid.hash,
            decrypt_key: root_cid.key,
        })?;
        let (port, handle) = spawn_test_server_with_auth(Arc::clone(&store)).await?;
        let client = reqwest::Client::new();

        for expected_already_pinned in [false, true] {
            let response = client
                .post(format!("http://127.0.0.1:{port}/api/pin-tree"))
                .basic_auth("test-user", Some("test-password"))
                .json(&json!({"nhash": nhash}))
                .send()
                .await?;
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            let body: serde_json::Value = response.json().await?;
            assert_eq!(body["already_pinned"], expected_already_pinned);
            assert!(body["indexed_hashes"].as_u64().unwrap_or_default() > 2);
        }

        assert!(store.is_pinned(&root_cid.hash)?);
        assert_eq!(store.list_pins_raw()?, vec![root_cid.hash]);
        assert_eq!(store.list_indexed_trees()?.len(), 1);
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
        assert_eq!(
            tree.get(&file_cid, None).await?,
            Some(expected_content),
            "encrypted descendant remains readable"
        );
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn pin_tree_missing_descendant_leaves_no_pin_or_index() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);
        let (root_cid, file_cid, _) = encrypted_test_directory(&store).await?;
        assert!(store.router().delete_sync(&file_cid.hash)?);
        let nhash = nhash_encode_full(&NHashData {
            hash: root_cid.hash,
            decrypt_key: root_cid.key,
        })?;
        let (port, handle) = spawn_test_server_with_auth(Arc::clone(&store)).await?;

        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/api/pin-tree"))
            .basic_auth("test-user", Some("test-password"))
            .json(&json!({"nhash": nhash}))
            .send()
            .await?;

        assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!store.is_pinned(&root_cid.hash)?);
        assert!(store.get_tree_meta(&root_cid.hash)?.is_none());
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn pin_tree_resource_failure_leaves_no_pin_or_index() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::with_options(
            temp_dir.path().join("db"),
            None,
            8,
        )?);
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
        let (cid, _) = tree.put(b"larger than the pin tree byte budget").await?;
        let nhash = nhash_encode_full(&NHashData {
            hash: cid.hash,
            decrypt_key: cid.key,
        })?;
        let (port, handle) = spawn_test_server_with_auth(Arc::clone(&store)).await?;

        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/api/pin-tree"))
            .basic_auth("test-user", Some("test-password"))
            .json(&json!({"nhash": nhash}))
            .send()
            .await?;

        assert_eq!(response.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
        assert!(!store.is_pinned(&cid.hash)?);
        assert!(store.get_tree_meta(&cid.hash)?.is_none());
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn unauthenticated_upload_batch_is_rejected_before_json_extraction() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);
        let (port, handle) = spawn_test_server(store).await?;

        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/upload/batch"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body("not valid json")
            .send()
            .await?;

        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn unauthenticated_single_upload_is_rejected_before_body_limit() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);
        let (port, handle) = spawn_test_server(store).await?;
        let declared_bytes = blossom::MAX_SINGLE_UPLOAD_BODY_BYTES + 1;
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
        stream
            .write_all(
                format!(
                    "PUT /upload HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {declared_bytes}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await?;

        // Send headers only: receiving a response proves authentication ran
        // before either buffering the declared body or enforcing its limit.
        let mut status_line = String::new();
        BufReader::new(stream).read_line(&mut status_line).await?;
        assert_eq!(status_line.trim_end(), "HTTP/1.1 401 Unauthorized");
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn upload_options_preflight_does_not_require_auth() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);
        let (port, handle) = spawn_test_server(store).await?;

        let response = reqwest::Client::new()
            .request(
                reqwest::Method::OPTIONS,
                format!("http://127.0.0.1:{port}/upload"),
            )
            .send()
            .await?;

        assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn virtual_tree_hosts_serve_root_assets_and_spa_fallbacks() -> Result<()> {
        clear_virtual_tree_hosts_for_test();

        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());

        let (index_cid, _) = tree
            .put(b"<!doctype html><title>Virtual host ok</title>")
            .await?;
        let (favicon_cid, _) = tree.put(b"ico").await?;
        let (main_js_cid, _) = tree.put(b"console.log('ok');").await?;
        let assets_dir = tree
            .put_directory(vec![
                DirEntry::from_cid("main.js", &main_js_cid).with_link_type(LinkType::File)
            ])
            .await?;
        let root_cid = tree
            .put_directory(vec![
                DirEntry::from_cid("index.html", &index_cid).with_link_type(LinkType::File),
                DirEntry::from_cid("favicon.ico", &favicon_cid).with_link_type(LinkType::File),
                DirEntry::from_cid("assets", &assets_dir).with_link_type(LinkType::Dir),
            ])
            .await?;
        let nhash = nhash_encode(&root_cid.hash)?;
        let host = "tree-test.htree.localhost";
        register_virtual_tree_host(host, &format!("/htree/{nhash}"));

        let (port, handle) = spawn_test_server(store).await?;
        let base_url = format!("http://127.0.0.1:{port}");
        let host_header = format!("{host}:{port}");
        let client = reqwest::Client::new();

        let root_response = client
            .get(format!("{base_url}/"))
            .header("Host", &host_header)
            .header("Accept", "text/html")
            .send()
            .await?;
        assert_eq!(root_response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            root_response.bytes().await?.as_ref(),
            b"<!doctype html><title>Virtual host ok</title>"
        );

        let favicon_response = client
            .get(format!("{base_url}/favicon.ico"))
            .header("Host", &host_header)
            .send()
            .await?;
        assert_eq!(favicon_response.status(), reqwest::StatusCode::OK);
        assert_eq!(favicon_response.bytes().await?.as_ref(), b"ico");

        let js_response = client
            .get(format!("{base_url}/assets/main.js"))
            .header("Host", &host_header)
            .send()
            .await?;
        assert_eq!(js_response.status(), reqwest::StatusCode::OK);
        assert_eq!(js_response.bytes().await?.as_ref(), b"console.log('ok');");

        let profile_response = client
            .get(format!("{base_url}/users/npub1example"))
            .header("Host", &host_header)
            .header("Accept", "text/html")
            .send()
            .await?;
        assert_eq!(profile_response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            profile_response.bytes().await?.as_ref(),
            b"<!doctype html><title>Virtual host ok</title>"
        );

        handle.abort();
        clear_virtual_tree_hosts_for_test();

        Ok(())
    }

    #[tokio::test]
    async fn iris_localhost_hosts_serve_immutable_and_named_sites() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());

        let (index_cid, _) = tree
            .put(b"<!doctype html><title>Iris localhost ok</title>")
            .await?;
        let (main_js_cid, _) = tree.put(b"console.log('iris localhost');").await?;
        let assets_dir = tree
            .put_directory(vec![
                DirEntry::from_cid("main.js", &main_js_cid).with_link_type(LinkType::File)
            ])
            .await?;
        let root_cid = tree
            .put_directory(vec![
                DirEntry::from_cid("index.html", &index_cid).with_link_type(LinkType::File),
                DirEntry::from_cid("assets", &assets_dir).with_link_type(LinkType::Dir),
            ])
            .await?;

        let nhash = nhash_encode(&root_cid.hash)?;
        let npub = Keys::generate().public_key().to_bech32()?;
        let site = "audio";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let server = HashtreeServer::new(Arc::clone(&store), "127.0.0.1:0".to_string())
            .with_public_plaintext_reads(true)
            .with_cached_tree_roots(vec![(format!("{npub}/{site}"), root_cid)]);
        let handle =
            tokio::spawn(async move { server.run_with_listener(listener).await.map(|_| ()) });
        let client = reqwest::Client::new();
        let base_url = format!("http://127.0.0.1:{port}");

        for host in [
            format!("{nhash}.iris.localhost:{port}"),
            format!("{site}.{npub}.iris.localhost:{port}"),
        ] {
            let root_response = client
                .get(format!("{base_url}/"))
                .header("Host", &host)
                .header("Accept", "text/html")
                .send()
                .await?;
            assert_eq!(root_response.status(), reqwest::StatusCode::OK, "{host}");
            assert_eq!(
                root_response.bytes().await?.as_ref(),
                b"<!doctype html><title>Iris localhost ok</title>"
            );

            let asset_response = client
                .get(format!("{base_url}/assets/main.js"))
                .header("Host", &host)
                .send()
                .await?;
            assert_eq!(asset_response.status(), reqwest::StatusCode::OK, "{host}");
            assert_eq!(
                asset_response.bytes().await?.as_ref(),
                b"console.log('iris localhost');"
            );
        }

        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn named_iris_site_resolves_from_fips_event_provider_without_relays() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
        let (index_cid, _) = tree.put(b"fips event provider site").await?;
        let root_cid = tree
            .put_directory(vec![
                DirEntry::from_cid("index.html", &index_cid).with_link_type(LinkType::File)
            ])
            .await?;
        let owner = Keys::generate();
        let site = "radio";
        let root_event = EventBuilder::new(Kind::Custom(30064), "")
            .tags(vec![
                nostr::Tag::identifier(site),
                nostr::Tag::custom(nostr::TagKind::custom("l"), vec!["hashtree".to_string()]),
                nostr::Tag::custom(nostr::TagKind::custom("hash"), vec![to_hex(&root_cid.hash)]),
            ])
            .sign_with_keys(&owner)?;
        let provider = Arc::new(StaticProvider {
            event: Some(VerifiedEvent::try_from(root_event)?),
            queries: AtomicUsize::new(0),
            publishes: AtomicUsize::new(0),
            mode: PubsubProviderMode::LocalOnly,
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let server = HashtreeServer::new(Arc::clone(&store), "127.0.0.1:0".to_string())
            .with_public_plaintext_reads(true)
            .with_nostr_provider(provider.clone());
        assert!(server.state.nostr_relay_urls.is_empty());
        assert_eq!(
            server.state.nostr_provider.as_ref().unwrap().mode(),
            PubsubProviderMode::LocalOnly
        );
        let handle =
            tokio::spawn(async move { server.run_with_listener(listener).await.map(|_| ()) });

        let npub = owner.public_key().to_bech32()?;
        let response = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/"))
            .header("Host", format!("{site}.{npub}.iris.localhost"))
            .header("Accept", "text/html")
            .send()
            .await?;

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.bytes().await?.as_ref(),
            b"fips event provider site"
        );
        assert_eq!(provider.queries.load(Ordering::Relaxed), 1);
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn loopback_event_publication_uses_configured_provider() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::TextNote, "provider test").sign_with_keys(&keys)?;
        let provider = Arc::new(StaticProvider {
            event: Some(VerifiedEvent::try_from(event.clone())?),
            queries: AtomicUsize::new(0),
            publishes: AtomicUsize::new(0),
            mode: PubsubProviderMode::LocalOnly,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let server = HashtreeServer::new(store, "127.0.0.1:0".to_string())
            .with_nostr_provider(provider.clone());
        assert!(server.state.nostr_relay_urls.is_empty());
        let handle =
            tokio::spawn(async move { server.run_with_listener(listener).await.map(|_| ()) });

        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/api/nostr/events"))
            .json(&event)
            .send()
            .await?;

        assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
        assert_eq!(provider.publishes.load(Ordering::Relaxed), 1);
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn loopback_root_publication_is_immediately_resolvable_without_provider_replay(
    ) -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);
        let keys = Keys::generate();
        let tree_name = "webvm-e2e";
        let root_hash = "11".repeat(32);
        let encryption_key = "22".repeat(32);
        let event = EventBuilder::new(Kind::Custom(30064), &root_hash)
            .tags(vec![
                nostr::Tag::identifier(tree_name),
                nostr::Tag::custom(nostr::TagKind::custom("l"), vec!["hashtree".to_string()]),
                nostr::Tag::custom(nostr::TagKind::custom("l"), vec!["git".to_string()]),
                nostr::Tag::custom(nostr::TagKind::custom("hash"), vec![root_hash.clone()]),
                nostr::Tag::custom(nostr::TagKind::custom("key"), vec![encryption_key.clone()]),
            ])
            .sign_with_keys(&keys)?;
        let provider = Arc::new(StaticProvider {
            event: None,
            queries: AtomicUsize::new(0),
            publishes: AtomicUsize::new(0),
            mode: PubsubProviderMode::LocalOnly,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let server = HashtreeServer::new(store, "127.0.0.1:0".to_string())
            .with_nostr_provider(provider.clone());
        assert!(server.state.nostr_relay_urls.is_empty());
        let handle =
            tokio::spawn(async move { server.run_with_listener(listener).await.map(|_| ()) });
        let client = reqwest::Client::new();

        let publish = client
            .post(format!("http://127.0.0.1:{port}/api/nostr/events"))
            .json(&event)
            .send()
            .await?;
        assert_eq!(publish.status(), reqwest::StatusCode::ACCEPTED);

        let npub = keys.public_key().to_bech32()?;
        let resolved = client
            .get(format!(
                "http://127.0.0.1:{port}/api/nostr/resolve/{npub}/{tree_name}?refresh=1"
            ))
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        assert_eq!(resolved["hash"], root_hash);
        assert_eq!(resolved["key_tag"], encryption_key);
        assert_eq!(resolved["source"], "local-relay");
        assert_eq!(provider.publishes.load(Ordering::Relaxed), 1);
        assert_eq!(provider.queries.load(Ordering::Relaxed), 1);
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn nostr_profile_route_returns_latest_metadata_event() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);
        let graph_store = {
            let _guard = crate::socialgraph::test_lock().await;
            crate::socialgraph::open_test_social_graph_store_with_mapsize(
                &temp_dir.path().join("relay-db"),
                Some(128 * 1024 * 1024),
            )?
        };
        let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store;
        let relay = Arc::new(NostrRelay::new(
            backend,
            temp_dir.path().to_path_buf(),
            HashSet::new(),
            None,
            NostrRelayConfig {
                spambox_db_max_bytes: 0,
                ..Default::default()
            },
        )?);

        let author = Keys::generate();
        let older = EventBuilder::new(
            Kind::Metadata,
            json!({ "name": "older", "about": "before" }).to_string(),
        )
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&author)?;
        let newer = EventBuilder::new(
            Kind::Metadata,
            json!({ "name": "newer", "about": "after" }).to_string(),
        )
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&author)?;

        relay.ingest_trusted_event(older).await?;
        relay.ingest_trusted_event(newer.clone()).await?;

        let (port, handle) = spawn_test_server_with_nostr_relay(store, relay).await?;
        let response = reqwest::get(format!(
            "http://127.0.0.1:{port}/api/nostr/profile/{}",
            author.public_key().to_hex()
        ))
        .await?;

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let payload: serde_json::Value = response.json().await?;
        assert_eq!(payload["profile"]["name"].as_str(), Some("newer"),);
        assert_eq!(payload["profile"]["about"].as_str(), Some("after"));
        assert_eq!(payload["created_at"].as_u64(), Some(20));
        let expected_event_id = newer.id.to_hex();
        assert_eq!(
            payload["event_id"].as_str(),
            Some(expected_event_id.as_str())
        );

        handle.abort();
        Ok(())
    }
}
