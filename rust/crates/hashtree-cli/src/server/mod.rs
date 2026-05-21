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
#[cfg(feature = "p2p")]
pub mod stun;
mod ui;
pub mod ws_relay;

use crate::nostr_relay::NostrRelay;
use crate::socialgraph;
use crate::storage::HashtreeStore;
use crate::webrtc::WebRTCState;
use anyhow::Result;
use axum::{
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post, put},
    Router,
};
use std::collections::{HashMap, HashSet};
use std::future;
use std::sync::{Arc, OnceLock, RwLock};
use tower_http::cors::CorsLayer;

pub use auth::{new_lookup_cache, AppState, AuthCredentials, CachedTreeRootEntry};

static VIRTUAL_TREE_HOSTS: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();
const DEFAULT_OPTIMISTIC_UPLOAD_QUEUE_BYTES: usize = 512 * 1024 * 1024;

fn virtual_tree_hosts() -> &'static RwLock<HashMap<String, String>> {
    VIRTUAL_TREE_HOSTS.get_or_init(|| RwLock::new(HashMap::new()))
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
    virtual_tree_hosts()
        .read()
        .ok()
        .and_then(|hosts| hosts.get(&normalized_host).cloned())
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
                http_webrtc_fetch: true,
                webrtc_peers: None,
                fips_transport: None,
                fetch_from_fips_peers: true,
                ws_relay: Arc::new(auth::WsRelayState::new()),
                max_upload_bytes: 5 * 1024 * 1024, // 5 MB default
                public_writes: true,               // Allow anyone with valid Nostr auth by default
                require_random_untrusted_ingest: true,
                optimistic_blossom_uploads: false,
                optimistic_upload_queue_bytes: DEFAULT_OPTIMISTIC_UPLOAD_QUEUE_BYTES,
                optimistic_upload_queue: Arc::new(tokio::sync::Semaphore::new(
                    DEFAULT_OPTIMISTIC_UPLOAD_QUEUE_BYTES,
                )),
                allowed_pubkeys: HashSet::new(), // No pubkeys allowed by default (use public_writes)
                upstream_blossom: Vec::new(),
                social_graph: None,
                social_graph_store: None,
                social_graph_root: None,
                socialgraph_snapshot_public: false,
                nostr_relay: None,
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

    pub fn with_http_webrtc_fetch(mut self, enabled: bool) -> Self {
        self.state.http_webrtc_fetch = enabled;
        self
    }

    pub fn with_fetch_from_fips_peers(mut self, enabled: bool) -> Self {
        self.state.fetch_from_fips_peers = enabled;
        self
    }

    pub fn with_fips_transport(
        mut self,
        transport: Arc<crate::fips_transport::DaemonFipsTransport>,
    ) -> Self {
        self.state.fips_transport = Some(transport);
        self
    }

    /// Set WebRTC state for P2P peer queries
    pub fn with_webrtc_peers(mut self, webrtc_state: Arc<WebRTCState>) -> Self {
        self.state.webrtc_peers = Some(webrtc_state);
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

    /// Set active upstream Nostr relays for HTTP resolver operations.
    pub fn with_nostr_relay_urls(mut self, relays: Vec<String>) -> Self {
        self.state.nostr_relay_urls = relays;
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
        let local_addr = listener.local_addr()?;

        // Public endpoints (no auth required)
        // Note: /:id serves both CID and blossom SHA256 hash lookups
        // The handler differentiates based on hash format (64 char hex = blossom)
        let state = self.state.clone();
        let public_routes = Router::new()
            .route("/", get(handlers::serve_root_or_virtual_host))
            .route("/ws", get(ws_relay::ws_data))
            .route("/ws/", get(ws_relay::ws_data))
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
                put(blossom::upload_blob).options(blossom::cors_preflight),
            )
            .route(
                "/list/:pubkey",
                get(blossom::list_blobs).options(blossom::cors_preflight),
            )
            // Hashtree API endpoints
            .route("/health", get(handlers::health_check))
            .route("/api/pins", get(handlers::list_pins))
            .route("/api/stats", get(handlers::storage_stats))
            .route("/api/peers", get(handlers::webrtc_peers))
            .route("/api/status", get(handlers::daemon_status))
            .route("/api/p2p/signal", post(handlers::p2p_signal))
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
            .route("/api/cache-tree-root", post(handlers::cache_tree_root))
            .route(
                "/api/clear-tree-root-cache",
                post(handlers::clear_tree_root_cache),
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

        let mut app = public_routes
            .merge(protected_routes)
            .layer(DefaultBodyLimit::max(10 * 1024 * 1024 * 1024)) // 10GB limit
            .layer(middleware::from_fn(status_metrics::record_http_status));

        if let Some(extra) = self.extra_routes {
            app = app.merge(extra.with_state(state));
        }

        if let Some(cors) = self.cors {
            app = app.layer(cors);
        }

        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown)
        .await?;

        Ok(local_addr.port())
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }
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
    use hashtree_core::{from_hex, nhash_encode, DirEntry, HashTree, HashTreeConfig, LinkType};
    use nostr::{EventBuilder, Keys, Kind, Timestamp};
    use serde_json::json;
    use tempfile::TempDir;

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
    async fn nostr_profile_route_returns_latest_metadata_event() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db"))?);
        let graph_store = {
            let _guard = crate::socialgraph::test_lock();
            crate::socialgraph::open_social_graph_store_with_mapsize(
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
            [],
        )
        .custom_created_at(Timestamp::from_secs(10))
        .to_event(&author)?;
        let newer = EventBuilder::new(
            Kind::Metadata,
            json!({ "name": "newer", "about": "after" }).to_string(),
            [],
        )
        .custom_created_at(Timestamp::from_secs(20))
        .to_event(&author)?;

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
