mod auth;
pub mod blossom;
mod handlers;
mod ws_relay;
mod mime;
#[cfg(feature = "p2p")]
pub mod stun;
mod ui;

use anyhow::Result;
use axum::{
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post, put},
    Router,
};
use crate::storage::HashtreeStore;
use crate::webrtc::WebRTCState;
use std::collections::HashSet;
use std::sync::Arc;

pub use auth::{AppState, AuthCredentials};

pub struct HashtreeServer {
    state: AppState,
    addr: String,
}

impl HashtreeServer {
    pub fn new(store: Arc<HashtreeStore>, addr: String) -> Self {
        Self {
            state: AppState {
                store,
                auth: None,
                webrtc_peers: None,
                ws_relay: Arc::new(auth::WsRelayState::new()),
                max_upload_bytes: 5 * 1024 * 1024, // 5 MB default
                public_writes: true, // Allow anyone with valid Nostr auth by default
                allowed_pubkeys: HashSet::new(), // No pubkeys allowed by default (use public_writes)
                upstream_blossom: Vec::new(),
            },
            addr,
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

    pub async fn run(self) -> Result<()> {
        // Public endpoints (no auth required)
        // Note: /:id serves both CID and blossom SHA256 hash lookups
        // The handler differentiates based on hash format (64 char hex = blossom)
        let public_routes = Router::new()
            .route("/", get(handlers::serve_root))
            .route("/ws/data", get(ws_relay::ws_data))
            // Nostr resolver endpoints - resolve npub/treename to content
            .route("/n/:pubkey/:treename", get(handlers::resolve_and_serve))
            // Direct npub route (clients should parse nhash and request by hex hash)
            .route("/npub1:rest", get(handlers::serve_npub))
            // Blossom endpoints (BUD-01, BUD-02)
            .route("/:id", get(handlers::serve_content_or_blob)
                .head(blossom::head_blob)
                .delete(blossom::delete_blob)
                .options(blossom::cors_preflight))
            .route("/upload", put(blossom::upload_blob)
                .options(blossom::cors_preflight))
            .route("/list/:pubkey", get(blossom::list_blobs)
                .options(blossom::cors_preflight))
            // Hashtree API endpoints
            .route("/health", get(handlers::health_check))
            .route("/api/pins", get(handlers::list_pins))
            .route("/api/stats", get(handlers::storage_stats))
            .route("/api/peers", get(handlers::webrtc_peers))
            .route("/api/status", get(handlers::daemon_status))
            .route("/api/socialgraph", get(handlers::socialgraph_stats))
            // Resolver API endpoints
            .route("/api/resolve/:pubkey/:treename", get(handlers::resolve_to_hash))
            .route("/api/trees/:pubkey", get(handlers::list_trees))
            .with_state(self.state.clone());

        // Protected endpoints (require auth if enabled)
        let protected_routes = Router::new()
            .route("/upload", post(handlers::upload_file))
            .route("/api/pin/:cid", post(handlers::pin_cid))
            .route("/api/unpin/:cid", post(handlers::unpin_cid))
            .route("/api/gc", post(handlers::garbage_collect))
            .layer(middleware::from_fn_with_state(
                self.state.clone(),
                auth::auth_middleware,
            ))
            .with_state(self.state);

        let app = public_routes
            .merge(protected_routes)
            .layer(DefaultBodyLimit::max(10 * 1024 * 1024 * 1024)); // 10GB limit

        let listener = tokio::net::TcpListener::bind(&self.addr).await?;
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        ).await?;

        Ok(())
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::HashtreeStore;
    use tempfile::TempDir;
    use std::path::Path;
    use hashtree_core::from_hex;

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
}
