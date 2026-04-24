use anyhow::{Context, Result};
use axum::Router;
use nostr::nips::nip19::ToBech32;
use nostr::Keys;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tower_http::cors::CorsLayer;

use crate::config::{ensure_keys, ensure_keys_in, parse_npub, pubkey_bytes, Config};
use crate::eviction::{spawn_background_eviction_task, BACKGROUND_EVICTION_INTERVAL};
use crate::nostr_relay::{NostrRelay, NostrRelayConfig};
use crate::server::{AppState, HashtreeServer};
use crate::socialgraph;
use crate::storage::HashtreeStore;

#[cfg(feature = "p2p")]
use crate::webrtc::{ContentStore, PeerClassifier, WebRTCManager, WebRTCState};
#[cfg(not(feature = "p2p"))]
use crate::WebRTCState;

#[cfg(feature = "p2p")]
struct PeerRouterRuntime {
    shutdown: Arc<tokio::sync::watch::Sender<bool>>,
    join: JoinHandle<()>,
}

struct BackgroundSyncRuntime {
    service: Arc<crate::sync::BackgroundSync>,
    join: Option<JoinHandle<()>>,
}

impl Drop for BackgroundSyncRuntime {
    fn drop(&mut self) {
        self.service.shutdown();
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }
}

struct BackgroundMirrorRuntime {
    service: Arc<crate::nostr_mirror::BackgroundNostrMirror>,
    join: Option<JoinHandle<()>>,
}

impl Drop for BackgroundMirrorRuntime {
    fn drop(&mut self) {
        self.service.shutdown();
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }
}

struct BackgroundServicesRuntime {
    crawler: Option<socialgraph::crawler::SocialGraphTaskHandles>,
    mirror: Option<BackgroundMirrorRuntime>,
    sync: Option<BackgroundSyncRuntime>,
}

impl Drop for BackgroundServicesRuntime {
    fn drop(&mut self) {
        if let Some(handles) = self.crawler.as_ref() {
            let _ = handles.shutdown_tx.send(true);
        }
        if let Some(runtime) = self.mirror.as_ref() {
            runtime.service.shutdown();
        }
        if let Some(runtime) = self.sync.as_ref() {
            runtime.service.shutdown();
        }
    }
}

impl BackgroundServicesRuntime {
    fn status(&self) -> EmbeddedBackgroundServicesStatus {
        EmbeddedBackgroundServicesStatus {
            crawler_active: self.crawler.is_some(),
            mirror_active: self.mirror.is_some(),
            sync_active: self.sync.is_some(),
        }
    }
}

struct EmbeddedServerRuntime {
    shutdown: Arc<Notify>,
    join: Option<JoinHandle<()>>,
}

pub struct EmbeddedServerController {
    runtime: Mutex<Option<EmbeddedServerRuntime>>,
}

impl EmbeddedServerController {
    pub fn new(shutdown: Arc<Notify>, join: JoinHandle<()>) -> Self {
        Self {
            runtime: Mutex::new(Some(EmbeddedServerRuntime {
                shutdown,
                join: Some(join),
            })),
        }
    }

    pub async fn shutdown(&self) {
        let mut runtime = self.runtime.lock().await;
        let Some(mut runtime) = runtime.take() else {
            return;
        };

        runtime.shutdown.notify_waiters();
        if let Some(mut join) = runtime.join.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(3), &mut join).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    tracing::warn!("Embedded server task ended with join error: {}", err)
                }
                Err(_) => {
                    tracing::warn!("Timed out waiting for embedded server shutdown");
                    join.abort();
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddedBackgroundServicesStatus {
    pub crawler_active: bool,
    pub mirror_active: bool,
    pub sync_active: bool,
}

pub struct EmbeddedBackgroundServicesController {
    keys: Keys,
    data_dir: PathBuf,
    store: Arc<HashtreeStore>,
    graph_store_concrete: Arc<socialgraph::SocialGraphStore>,
    graph_store: Arc<dyn socialgraph::SocialGraphBackend>,
    spambox: Option<Arc<dyn socialgraph::SocialGraphBackend>>,
    webrtc_state: Option<Arc<WebRTCState>>,
    runtime: Mutex<BackgroundServicesRuntime>,
}

impl EmbeddedBackgroundServicesController {
    const MIRROR_PUBLISH_RELAY_PRIORITY: &[&str] = &[
        "wss://temp.iris.to",
        "wss://vault.iris.to",
        "wss://relay.damus.io",
        "wss://relay.primal.net",
    ];
    const MIRROR_PUBLISH_RELAY_BLOCKLIST: &[&str] =
        &["wss://graph-relay.iris.to", "wss://upload.iris.to/nostr"];

    fn mirror_publish_relays(active_relays: &[String], _bind_address: &str) -> Vec<String> {
        let mut seen = HashSet::new();
        let active_relays = active_relays
            .iter()
            .filter(|relay| seen.insert((*relay).clone()))
            .cloned()
            .collect::<Vec<_>>();
        if active_relays.is_empty() {
            return Vec::new();
        }
        let active_relay_set = active_relays.iter().cloned().collect::<HashSet<_>>();

        let preferred = Self::MIRROR_PUBLISH_RELAY_PRIORITY
            .iter()
            .filter(|relay| active_relay_set.contains(**relay))
            .map(|relay| (*relay).to_string())
            .collect::<Vec<_>>();
        if !preferred.is_empty() {
            return preferred;
        }

        let filtered = active_relays
            .iter()
            .filter(|relay| !Self::MIRROR_PUBLISH_RELAY_BLOCKLIST.contains(&relay.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if !filtered.is_empty() {
            return filtered;
        }

        active_relays
    }

    pub fn new(
        keys: Keys,
        data_dir: PathBuf,
        store: Arc<HashtreeStore>,
        graph_store_concrete: Arc<socialgraph::SocialGraphStore>,
        graph_store: Arc<dyn socialgraph::SocialGraphBackend>,
        spambox: Option<Arc<dyn socialgraph::SocialGraphBackend>>,
        webrtc_state: Option<Arc<WebRTCState>>,
    ) -> Self {
        Self {
            keys,
            data_dir,
            store,
            graph_store_concrete,
            graph_store,
            spambox,
            webrtc_state,
            runtime: Mutex::new(BackgroundServicesRuntime {
                crawler: None,
                mirror: None,
                sync: None,
            }),
        }
    }

    pub async fn status(&self) -> EmbeddedBackgroundServicesStatus {
        self.runtime.lock().await.status()
    }

    pub async fn shutdown(&self) {
        let mut runtime = self.runtime.lock().await;
        Self::shutdown_crawler(&mut runtime.crawler).await;
        Self::shutdown_mirror(&mut runtime.mirror).await;
        Self::shutdown_sync(&mut runtime.sync).await;
    }

    async fn shutdown_crawler(crawler: &mut Option<socialgraph::crawler::SocialGraphTaskHandles>) {
        let Some(handles) = crawler.take() else {
            return;
        };

        let _ = handles.shutdown_tx.send(true);

        let mut crawl_handle = handles.crawl_handle;
        match tokio::time::timeout(std::time::Duration::from_secs(3), &mut crawl_handle).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::warn!("Crawler task ended with join error: {}", err),
            Err(_) => {
                tracing::warn!("Timed out waiting for crawler task shutdown");
                crawl_handle.abort();
            }
        }

        let mut local_list_handle = handles.local_list_handle;
        match tokio::time::timeout(std::time::Duration::from_secs(3), &mut local_list_handle).await
        {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::warn!("Local list task ended with join error: {}", err),
            Err(_) => {
                tracing::warn!("Timed out waiting for local list task shutdown");
                local_list_handle.abort();
            }
        }
    }

    async fn shutdown_sync(sync: &mut Option<BackgroundSyncRuntime>) {
        let Some(mut runtime) = sync.take() else {
            return;
        };

        runtime.service.shutdown();
        if let Some(mut join) = runtime.join.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(3), &mut join).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    tracing::warn!("Background sync task ended with join error: {}", err)
                }
                Err(_) => {
                    tracing::warn!("Timed out waiting for background sync shutdown");
                    join.abort();
                }
            }
        }
    }

    async fn shutdown_mirror(mirror: &mut Option<BackgroundMirrorRuntime>) {
        let Some(mut runtime) = mirror.take() else {
            return;
        };

        runtime.service.shutdown();
        if let Some(mut join) = runtime.join.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(3), &mut join).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    tracing::warn!("Background mirror task ended with join error: {}", err)
                }
                Err(_) => {
                    tracing::warn!("Timed out waiting for background mirror shutdown");
                    join.abort();
                }
            }
        }
    }

    pub async fn apply_config(&self, config: &Config) -> Result<EmbeddedBackgroundServicesStatus> {
        let mut runtime = self.runtime.lock().await;

        Self::shutdown_crawler(&mut runtime.crawler).await;
        Self::shutdown_mirror(&mut runtime.mirror).await;
        Self::shutdown_sync(&mut runtime.sync).await;

        if !config.server.mode.background_services_enabled() {
            return Ok(runtime.status());
        }

        let active_relays = config.nostr.active_relays();

        if config.nostr.enabled
            && config.nostr.social_graph_crawl_depth > 0
            && !active_relays.is_empty()
        {
            runtime.crawler = Some(socialgraph::crawler::spawn_social_graph_tasks(
                self.graph_store.clone(),
                self.keys.clone(),
                active_relays.clone(),
                config.nostr.social_graph_crawl_depth,
                self.spambox.clone(),
                self.data_dir.clone(),
            ));

            let service = Arc::new(
                crate::nostr_mirror::BackgroundNostrMirror::new(
                    crate::nostr_mirror::NostrMirrorConfig {
                        relays: active_relays.clone(),
                        publish_relays: Self::mirror_publish_relays(
                            &active_relays,
                            &config.server.bind_address,
                        ),
                        blossom_write_servers: config.blossom.all_write_servers(),
                        max_follow_distance: config.nostr.social_graph_crawl_depth,
                        overmute_threshold: config.nostr.overmute_threshold,
                        require_negentropy: config.nostr.negentropy_only,
                        kinds: config.nostr.mirror_kinds.clone(),
                        history_sync_author_chunk_size: config
                            .nostr
                            .history_sync_author_chunk_size
                            .max(1),
                        history_sync_per_author_event_limit: config
                            .nostr
                            .history_sync_per_author_event_limit
                            .max(1),
                        missing_profile_backfill_batch_size: config
                            .nostr
                            .history_sync_author_chunk_size
                            .max(1),
                        history_sync_on_reconnect: config.nostr.history_sync_on_reconnect,
                        full_text_note_history_follow_distance: config
                            .nostr
                            .full_text_note_history_follow_distance,
                        full_text_note_history_max_relay_pages: config
                            .nostr
                            .full_text_note_history_max_relay_pages
                            .max(1),
                        ..crate::nostr_mirror::NostrMirrorConfig::default()
                    },
                    self.store.clone(),
                    self.graph_store_concrete.clone(),
                    Some(
                        nostr_sdk::Keys::parse(&self.keys.secret_key().to_bech32()?)
                            .context("Failed to parse keys for background nostr mirror")?,
                    ),
                )
                .await
                .context("Failed to create background nostr mirror")?,
            );
            let service_for_task = service.clone();
            let join = tokio::task::spawn_blocking(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build background nostr mirror runtime");
                runtime.block_on(async {
                    if let Err(err) = service_for_task.run().await {
                        tracing::error!("Background nostr mirror error: {:#}", err);
                    }
                });
            });
            runtime.mirror = Some(BackgroundMirrorRuntime {
                service,
                join: Some(join),
            });
        }

        let has_pinned_refs = self
            .store
            .list_pinned_refs()
            .map(|refs| !refs.is_empty())
            .unwrap_or(false);
        let has_tracked_authors = self
            .store
            .list_tracked_authors()
            .map(|authors| !authors.is_empty())
            .unwrap_or(false);

        if config.sync.enabled
            && (config.sync.sync_own
                || config.sync.sync_followed
                || has_pinned_refs
                || has_tracked_authors)
            && !active_relays.is_empty()
        {
            let sync_config = crate::sync::SyncConfig {
                sync_own: config.sync.sync_own,
                sync_followed: config.sync.sync_followed,
                relays: active_relays,
                max_concurrent: config.sync.max_concurrent,
                webrtc_timeout_ms: config.sync.webrtc_timeout_ms,
                blossom_timeout_ms: config.sync.blossom_timeout_ms,
            };

            let sync_keys = nostr_sdk::Keys::parse(&self.keys.secret_key().to_bech32()?)
                .context("Failed to parse keys for sync")?;
            let service = Arc::new(
                crate::sync::BackgroundSync::new(
                    sync_config,
                    self.store.clone(),
                    sync_keys,
                    self.webrtc_state.clone(),
                )
                .await
                .context("Failed to create background sync service")?,
            );
            let contacts_file = self.data_dir.join("contacts.json");
            let service_for_task = service.clone();
            let join = tokio::spawn(async move {
                if let Err(err) = service_for_task.run(contacts_file).await {
                    tracing::error!("Background sync error: {}", err);
                }
            });
            runtime.sync = Some(BackgroundSyncRuntime {
                service,
                join: Some(join),
            });
        }

        Ok(runtime.status())
    }
}

#[cfg(feature = "p2p")]
pub struct EmbeddedPeerRouterController {
    keys: Keys,
    state: Arc<WebRTCState>,
    store: Arc<dyn ContentStore>,
    peer_classifier: PeerClassifier,
    nostr_relay: Arc<NostrRelay>,
    runtime: Mutex<Option<PeerRouterRuntime>>,
}

#[cfg(feature = "p2p")]
impl EmbeddedPeerRouterController {
    pub fn new(
        keys: Keys,
        state: Arc<WebRTCState>,
        store: Arc<dyn ContentStore>,
        peer_classifier: PeerClassifier,
        nostr_relay: Arc<NostrRelay>,
    ) -> Self {
        Self {
            keys,
            state,
            store,
            peer_classifier,
            nostr_relay,
            runtime: Mutex::new(None),
        }
    }

    pub fn state(&self) -> Arc<WebRTCState> {
        self.state.clone()
    }

    pub async fn apply_config(&self, config: &Config) -> Result<bool> {
        let mut runtime = self.runtime.lock().await;
        if let Some(runtime_handle) = runtime.take() {
            let _ = runtime_handle.shutdown.send(true);
            let mut join = runtime_handle.join;
            match tokio::time::timeout(std::time::Duration::from_secs(3), &mut join).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    tracing::warn!("Peer router task ended with join error: {}", err);
                }
                Err(_) => {
                    tracing::warn!("Timed out waiting for peer router shutdown");
                    join.abort();
                }
            }
        }

        self.state.reset_runtime_state().await;

        if !crate::p2p_common::peer_router_enabled(config) {
            return Ok(false);
        }

        let webrtc_config = crate::p2p_common::default_webrtc_config(config);
        let mut manager = if config.server.mode.hash_get_enabled() {
            WebRTCManager::new_with_state_and_store_and_classifier(
                self.keys.clone(),
                webrtc_config,
                self.state.clone(),
                self.store.clone(),
                self.peer_classifier.clone(),
            )
        } else {
            let mut manager =
                WebRTCManager::new_with_state(self.keys.clone(), webrtc_config, self.state.clone());
            manager.set_peer_classifier(self.peer_classifier.clone());
            manager
        };
        manager
            .set_nostr_relay(self.nostr_relay.clone() as hashtree_network::SharedMeshRelayClient);
        let shutdown = manager.shutdown_signal();
        let join = tokio::spawn(async move {
            if let Err(err) = manager.run().await {
                tracing::error!("Peer router error: {}", err);
            }
        });
        *runtime = Some(PeerRouterRuntime { shutdown, join });
        Ok(true)
    }

    pub async fn shutdown(&self) {
        let mut runtime = self.runtime.lock().await;
        let Some(runtime_handle) = runtime.take() else {
            return;
        };

        let _ = runtime_handle.shutdown.send(true);
        let mut join = runtime_handle.join;
        match tokio::time::timeout(std::time::Duration::from_secs(3), &mut join).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::warn!("Peer router task ended with join error: {}", err),
            Err(_) => {
                tracing::warn!("Timed out waiting for peer router shutdown");
                join.abort();
            }
        }

        self.state.reset_runtime_state().await;
    }
}

pub struct EmbeddedDaemonController {
    server_controller: Arc<EmbeddedServerController>,
    #[cfg(feature = "p2p")]
    peer_router_controller: Option<Arc<EmbeddedPeerRouterController>>,
    background_services_controller: Option<Arc<EmbeddedBackgroundServicesController>>,
}

impl EmbeddedDaemonController {
    #[cfg(feature = "p2p")]
    pub fn new(
        server_controller: Arc<EmbeddedServerController>,
        peer_router_controller: Option<Arc<EmbeddedPeerRouterController>>,
        background_services_controller: Option<Arc<EmbeddedBackgroundServicesController>>,
    ) -> Self {
        Self {
            server_controller,
            #[cfg(feature = "p2p")]
            peer_router_controller,
            background_services_controller,
        }
    }

    #[cfg(not(feature = "p2p"))]
    pub fn new(
        server_controller: Arc<EmbeddedServerController>,
        background_services_controller: Option<Arc<EmbeddedBackgroundServicesController>>,
    ) -> Self {
        Self {
            server_controller,
            background_services_controller,
        }
    }

    pub async fn shutdown(&self) {
        self.server_controller.shutdown().await;
        if let Some(controller) = self.background_services_controller.as_ref() {
            controller.shutdown().await;
        }
        #[cfg(feature = "p2p")]
        if let Some(controller) = self.peer_router_controller.as_ref() {
            controller.shutdown().await;
        }
    }
}

pub struct EmbeddedDaemonOptions {
    pub config: Config,
    pub data_dir: PathBuf,
    pub config_dir: Option<PathBuf>,
    pub bind_address: String,
    pub relays: Option<Vec<String>>,
    pub extra_routes: Option<Router<AppState>>,
    pub cors: Option<CorsLayer>,
}

pub struct EmbeddedDaemonInfo {
    pub addr: String,
    pub port: u16,
    pub npub: String,
    pub store: Arc<HashtreeStore>,
    pub daemon_controller: Arc<EmbeddedDaemonController>,
    #[allow(dead_code)]
    pub webrtc_state: Option<Arc<WebRTCState>>,
    #[cfg(feature = "p2p")]
    #[allow(dead_code)]
    pub peer_router_controller: Option<Arc<EmbeddedPeerRouterController>>,
    #[allow(dead_code)]
    pub background_services_controller: Option<Arc<EmbeddedBackgroundServicesController>>,
}

pub async fn start_embedded(opts: EmbeddedDaemonOptions) -> Result<EmbeddedDaemonInfo> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut config = opts.config;
    if let Some(relays) = opts.relays {
        config.nostr.relays = relays;
        config.nostr.enabled = !config.nostr.relays.is_empty();
    }

    let max_size_bytes = config.storage.max_size_gb * 1024 * 1024 * 1024;
    let nostr_db_max_bytes = config
        .nostr
        .db_max_size_gb
        .saturating_mul(1024 * 1024 * 1024);
    let spambox_db_max_bytes = config
        .nostr
        .spambox_max_size_gb
        .saturating_mul(1024 * 1024 * 1024);

    let store = Arc::new(HashtreeStore::with_options(
        &opts.data_dir,
        config.storage.s3.as_ref(),
        max_size_bytes,
    )?);

    let (keys, _was_generated) = if let Some(config_dir) = opts.config_dir.as_ref() {
        ensure_keys_in(config_dir, Some(&opts.data_dir), Some(&config))?
    } else {
        ensure_keys()?
    };
    let pk_bytes = pubkey_bytes(&keys);
    let npub = keys
        .public_key()
        .to_bech32()
        .context("Failed to encode npub")?;

    let mut allowed_pubkeys: HashSet<String> = HashSet::new();
    allowed_pubkeys.insert(hex::encode(pk_bytes));
    for npub_str in &config.nostr.allowed_npubs {
        if let Ok(pk) = parse_npub(npub_str) {
            allowed_pubkeys.insert(hex::encode(pk));
        } else {
            tracing::warn!("Invalid npub in allowed_npubs: {}", npub_str);
        }
    }

    let graph_store = socialgraph::open_social_graph_store_with_storage(
        &opts.data_dir,
        store.store_arc(),
        Some(nostr_db_max_bytes),
    )
    .context("Failed to initialize social graph store")?;
    graph_store.set_profile_index_overmute_threshold(config.nostr.overmute_threshold);

    let social_graph_root_bytes = if let Some(ref root_npub) = config.nostr.socialgraph_root {
        parse_npub(root_npub).unwrap_or(pk_bytes)
    } else {
        pk_bytes
    };
    socialgraph::set_social_graph_root(&graph_store, &social_graph_root_bytes);
    socialgraph::sync_local_list_files_force(graph_store.as_ref(), &opts.data_dir, &keys)
        .context("Failed to sync local social graph lists")?;
    let social_graph_store: Arc<dyn socialgraph::SocialGraphBackend> = graph_store.clone();

    let social_graph = Arc::new(socialgraph::SocialGraphAccessControl::new(
        Arc::clone(&social_graph_store),
        config.nostr.max_write_distance,
        allowed_pubkeys.clone(),
    ));

    let nostr_relay_config = NostrRelayConfig {
        spambox_db_max_bytes,
        ..Default::default()
    };
    let mut public_event_pubkeys = HashSet::new();
    public_event_pubkeys.insert(hex::encode(pk_bytes));
    let nostr_relay = Arc::new(
        NostrRelay::new(
            Arc::clone(&social_graph_store),
            opts.data_dir.clone(),
            public_event_pubkeys,
            Some(social_graph.clone()),
            nostr_relay_config,
        )
        .context("Failed to initialize Nostr relay")?,
    );

    let crawler_spambox = if spambox_db_max_bytes == 0 {
        None
    } else {
        let spam_dir = opts.data_dir.join("socialgraph_spambox");
        match socialgraph::open_social_graph_store_at_path(&spam_dir, Some(spambox_db_max_bytes)) {
            Ok(store) => Some(store),
            Err(err) => {
                tracing::warn!("Failed to open social graph spambox for crawler: {}", err);
                None
            }
        }
    };
    let crawler_spambox_backend = crawler_spambox
        .clone()
        .map(|store| store as Arc<dyn socialgraph::SocialGraphBackend>);

    #[cfg(feature = "p2p")]
    let (webrtc_state, peer_router_controller): (
        Option<Arc<WebRTCState>>,
        Option<Arc<EmbeddedPeerRouterController>>,
    ) = {
        let router_config = crate::p2p_common::default_webrtc_config(&config);
        let peer_classifier = crate::p2p_common::build_peer_classifier(
            opts.data_dir.clone(),
            Arc::clone(&social_graph_store),
        );
        let cashu_payment_client =
            if config.cashu.default_mint.is_some() || !config.cashu.accepted_mints.is_empty() {
                match crate::cashu_helper::CashuHelperClient::discover(opts.data_dir.clone()) {
                    Ok(client) => {
                        Some(Arc::new(client) as Arc<dyn crate::cashu_helper::CashuPaymentClient>)
                    }
                    Err(err) => {
                        tracing::warn!(
                        "Cashu settlement helper unavailable; paid retrieval stays disabled: {}",
                        err
                    );
                        None
                    }
                }
            } else {
                None
            };
        let cashu_mint_metadata =
            if config.cashu.default_mint.is_some() || !config.cashu.accepted_mints.is_empty() {
                let metadata_path = crate::webrtc::cashu_mint_metadata_path(&opts.data_dir);
                match crate::webrtc::CashuMintMetadataStore::load(metadata_path) {
                    Ok(store) => Some(store),
                    Err(err) => {
                        tracing::warn!(
                        "Failed to load Cashu mint metadata; falling back to in-memory state: {}",
                        err
                    );
                        Some(crate::webrtc::CashuMintMetadataStore::in_memory())
                    }
                }
            } else {
                None
            };

        let state = Arc::new(WebRTCState::new_with_routing_and_cashu(
            router_config.request_selection_strategy,
            router_config.request_fairness_enabled,
            router_config.request_dispatch,
            std::time::Duration::from_millis(router_config.message_timeout_ms),
            crate::webrtc::CashuRoutingConfig::from(&config.cashu),
            cashu_payment_client,
            cashu_mint_metadata,
        ));
        let controller = Arc::new(EmbeddedPeerRouterController::new(
            keys.clone(),
            state.clone(),
            Arc::clone(&store) as Arc<dyn ContentStore>,
            peer_classifier,
            nostr_relay.clone(),
        ));
        controller.apply_config(&config).await?;
        (Some(state), Some(controller))
    };

    #[cfg(not(feature = "p2p"))]
    let webrtc_state: Option<Arc<crate::webrtc::WebRTCState>> = None;

    let background_services_controller = Arc::new(EmbeddedBackgroundServicesController::new(
        keys.clone(),
        opts.data_dir.clone(),
        Arc::clone(&store),
        graph_store.clone(),
        Arc::clone(&social_graph_store),
        crawler_spambox_backend,
        webrtc_state.clone(),
    ));
    background_services_controller.apply_config(&config).await?;

    let upstream_blossom = config.blossom.all_read_servers();
    let active_nostr_relays = config.nostr.active_relays();

    let mut server = HashtreeServer::new(Arc::clone(&store), opts.bind_address.clone())
        .with_server_mode(config.server.mode)
        .with_hash_get_enabled(config.server.mode.hash_get_enabled())
        .with_allowed_pubkeys(allowed_pubkeys.clone())
        .with_max_upload_bytes((config.blossom.max_upload_mb as usize) * 1024 * 1024)
        .with_public_writes(config.server.public_writes)
        .with_upstream_blossom(upstream_blossom)
        .with_nostr_relay_urls(active_nostr_relays)
        .with_social_graph(social_graph)
        .with_socialgraph_snapshot(
            Arc::clone(&social_graph_store),
            social_graph_root_bytes,
            config.server.socialgraph_snapshot_public,
        )
        .with_nostr_relay(nostr_relay.clone());

    if crate::p2p_common::peer_router_enabled(&config) {
        if let Some(ref state) = webrtc_state {
            server = server.with_webrtc_peers(state.clone());
        }
    }

    if let Some(extra) = opts.extra_routes {
        server = server.with_extra_routes(extra);
    }
    if let Some(cors) = opts.cors {
        server = server.with_cors(cors);
    }

    spawn_background_eviction_task(
        Arc::clone(&store),
        BACKGROUND_EVICTION_INTERVAL,
        "embedded daemon",
    );

    let listener = TcpListener::bind(&opts.bind_address).await?;
    let local_addr = listener.local_addr()?;
    let actual_addr = format!("{}:{}", local_addr.ip(), local_addr.port());

    let server_shutdown = Arc::new(Notify::new());
    let server_shutdown_for_task = Arc::clone(&server_shutdown);
    let server_join = tokio::spawn(async move {
        if let Err(e) = server
            .run_with_listener_until(listener, async move {
                server_shutdown_for_task.notified().await;
            })
            .await
        {
            tracing::error!("Embedded daemon server error: {}", e);
        }
    });
    let server_controller = Arc::new(EmbeddedServerController::new(server_shutdown, server_join));
    #[cfg(feature = "p2p")]
    let daemon_controller = Arc::new(EmbeddedDaemonController::new(
        server_controller,
        peer_router_controller.clone(),
        Some(background_services_controller.clone()),
    ));
    #[cfg(not(feature = "p2p"))]
    let daemon_controller = Arc::new(EmbeddedDaemonController::new(
        server_controller,
        Some(background_services_controller.clone()),
    ));

    tracing::info!(
        "Embedded daemon started on {}, identity {}",
        actual_addr,
        npub
    );

    Ok(EmbeddedDaemonInfo {
        addr: actual_addr,
        port: local_addr.port(),
        npub,
        store,
        daemon_controller,
        webrtc_state,
        #[cfg(feature = "p2p")]
        peer_router_controller,
        background_services_controller: Some(background_services_controller),
    })
}

#[cfg(test)]
mod tests {
    use super::EmbeddedBackgroundServicesController;

    #[test]
    fn mirror_publish_relays_prefers_known_root_publish_relays() {
        let relays = EmbeddedBackgroundServicesController::mirror_publish_relays(
            &[
                "wss://graph-relay.iris.to".to_string(),
                "wss://relay.example".to_string(),
                "wss://relay.damus.io".to_string(),
                "wss://temp.iris.to".to_string(),
                "wss://vault.iris.to".to_string(),
                "wss://upload.iris.to/nostr".to_string(),
            ],
            "0.0.0.0:8080",
        );
        assert_eq!(
            relays,
            vec![
                "wss://temp.iris.to".to_string(),
                "wss://vault.iris.to".to_string(),
                "wss://relay.damus.io".to_string(),
            ]
        );
    }

    #[test]
    fn mirror_publish_relays_filters_known_bad_publish_targets_when_no_preferred_remain() {
        let relays = EmbeddedBackgroundServicesController::mirror_publish_relays(
            &[
                "wss://graph-relay.iris.to".to_string(),
                "wss://relay.snort.social".to_string(),
                "wss://relay.nostr.band".to_string(),
                "wss://upload.iris.to/nostr".to_string(),
            ],
            "0.0.0.0:8080",
        );
        assert_eq!(
            relays,
            vec![
                "wss://relay.snort.social".to_string(),
                "wss://relay.nostr.band".to_string(),
            ]
        );
    }
}
