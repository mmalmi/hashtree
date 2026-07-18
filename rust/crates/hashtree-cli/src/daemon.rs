use anyhow::{Context, Result};
use axum::Router;
use hashtree_core::Cid;
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
    runtime: Mutex<BackgroundServicesRuntime>,
}

impl EmbeddedBackgroundServicesController {
    const MIRROR_PUBLISH_RELAY_PRIORITY: &[&str] = &[
        "wss://nos.lol",
        "wss://temp.iris.to",
        "wss://vault.iris.to",
        "wss://relay.damus.io",
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
        let filtered = active_relays
            .iter()
            .filter(|relay| !Self::MIRROR_PUBLISH_RELAY_BLOCKLIST.contains(&relay.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if filtered.is_empty() {
            return active_relays;
        }

        let mut selected = Vec::new();
        let mut selected_set = HashSet::new();
        for relay in Self::MIRROR_PUBLISH_RELAY_PRIORITY {
            if filtered.iter().any(|active| active == relay) {
                selected.push((*relay).to_string());
                selected_set.insert((*relay).to_string());
            }
        }
        for relay in filtered {
            if selected_set.insert(relay.clone()) {
                selected.push(relay);
            }
        }

        selected
    }

    pub fn new(
        keys: Keys,
        data_dir: PathBuf,
        store: Arc<HashtreeStore>,
        graph_store_concrete: Arc<socialgraph::SocialGraphStore>,
        graph_store: Arc<dyn socialgraph::SocialGraphBackend>,
        spambox: Option<Arc<dyn socialgraph::SocialGraphBackend>>,
    ) -> Self {
        Self {
            keys,
            data_dir,
            store,
            graph_store_concrete,
            graph_store,
            spambox,
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

    fn nostr_mirror_config(
        config: &Config,
        active_relays: &[String],
    ) -> crate::nostr_mirror::NostrMirrorConfig {
        crate::nostr_mirror::NostrMirrorConfig {
            relays: active_relays.to_vec(),
            publish_relays: Self::mirror_publish_relays(active_relays, &config.server.bind_address),
            blossom_write_servers: config.blossom.all_write_servers(),
            max_follow_distance: config
                .nostr
                .mirror_max_follow_distance
                .unwrap_or(config.nostr.social_graph_crawl_depth),
            overmute_threshold: config.nostr.overmute_threshold,
            require_negentropy: config.nostr.negentropy_only,
            kinds: config.nostr.mirror_kinds.clone(),
            history_sync_author_chunk_size: config.nostr.history_sync_author_chunk_size.max(1),
            history_sync_per_author_event_limit: config
                .nostr
                .history_sync_per_author_event_limit
                .max(1),
            missing_profile_backfill_batch_size: config.nostr.history_sync_author_chunk_size.max(1),
            history_sync_on_reconnect: config.nostr.history_sync_on_reconnect,
            full_text_note_history_follow_distance: config
                .nostr
                .full_text_note_history_follow_distance,
            full_text_note_history_max_relay_pages: config
                .nostr
                .full_text_note_history_max_relay_pages,
            archive_history_follow_distance: config.nostr.archive_history_follow_distance,
            archive_history_max_relay_pages: config.nostr.archive_history_max_relay_pages,
            ..crate::nostr_mirror::NostrMirrorConfig::default()
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
                    Self::nostr_mirror_config(config, &active_relays),
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

        if config.sync.enabled && !active_relays.is_empty() {
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
            let should_sync = config.sync.sync_own
                || config.sync.sync_followed
                || has_pinned_refs
                || has_tracked_authors;
            if !should_sync {
                return Ok(runtime.status());
            }

            let sync_config = crate::sync::SyncConfig {
                sync_own: config.sync.sync_own,
                sync_followed: config.sync.sync_followed,
                relays: active_relays,
                max_concurrent: config.sync.max_concurrent,
                blossom_timeout_ms: config.sync.blossom_timeout_ms,
            };

            let sync_keys = nostr_sdk::Keys::parse(&self.keys.secret_key().to_bech32()?)
                .context("Failed to parse keys for sync")?;
            let service = Arc::new(
                crate::sync::BackgroundSync::new(sync_config, self.store.clone(), sync_keys)
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

pub struct EmbeddedDaemonController {
    server_controller: Arc<EmbeddedServerController>,
    fips_handle: Option<Arc<crate::fips_transport::DaemonFipsHandle>>,
    #[cfg(feature = "experimental-decentralized-pubsub")]
    nostr_pubsub_handle: Option<Arc<crate::fips_transport::DaemonNostrPubsubHandle>>,
    background_services_controller: Option<Arc<EmbeddedBackgroundServicesController>>,
}

impl EmbeddedDaemonController {
    pub fn new(
        server_controller: Arc<EmbeddedServerController>,
        fips_handle: Option<Arc<crate::fips_transport::DaemonFipsHandle>>,
        #[cfg(feature = "experimental-decentralized-pubsub")] nostr_pubsub_handle: Option<
            Arc<crate::fips_transport::DaemonNostrPubsubHandle>,
        >,
        background_services_controller: Option<Arc<EmbeddedBackgroundServicesController>>,
    ) -> Self {
        Self {
            server_controller,
            fips_handle,
            #[cfg(feature = "experimental-decentralized-pubsub")]
            nostr_pubsub_handle,
            background_services_controller,
        }
    }

    pub async fn shutdown(&self) {
        self.server_controller.shutdown().await;
        #[cfg(feature = "experimental-decentralized-pubsub")]
        if let Some(handle) = self.nostr_pubsub_handle.as_ref() {
            handle.shutdown();
        }
        if let Some(handle) = self.fips_handle.as_ref() {
            handle.shutdown().await;
        }
        if let Some(controller) = self.background_services_controller.as_ref() {
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
    pub initial_tree_roots: Vec<(String, Cid)>,
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
    pub background_services_controller: Option<Arc<EmbeddedBackgroundServicesController>>,
}

pub async fn start_embedded(opts: EmbeddedDaemonOptions) -> Result<EmbeddedDaemonInfo> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut config = opts.config;
    config.server.bind_address = opts.bind_address.clone();
    if let Some(relays) = opts.relays {
        config.nostr.relays = relays;
        config.nostr.enabled = embedded_nostr_enabled_after_relay_override(&config);
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

    let store = Arc::new(HashtreeStore::with_embedded_options(
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

    let graph_store = socialgraph::open_embedded_social_graph_store_with_storage(
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
    let fips_peer_ids = crate::fips_transport::fips_peer_ids_from_pubkeys(
        socialgraph::get_follows(graph_store.as_ref(), &pk_bytes),
    );
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
    let nostr_relay = if config.nostr.enabled {
        let mut public_event_pubkeys = HashSet::new();
        public_event_pubkeys.insert(hex::encode(pk_bytes));
        Some(Arc::new(
            NostrRelay::new(
                Arc::clone(&social_graph_store),
                opts.data_dir.clone(),
                public_event_pubkeys,
                Some(social_graph.clone()),
                nostr_relay_config,
            )
            .map(|relay| {
                relay.with_historical_nostr_index(store.store_arc(), opts.data_dir.clone())
            })
            .context("Failed to initialize Nostr relay")?,
        ))
    } else {
        None
    };

    let crawler_spambox = if config.nostr.enabled && spambox_db_max_bytes != 0 {
        let spam_dir = opts.data_dir.join("socialgraph_spambox");
        match socialgraph::open_embedded_social_graph_store_at_path(
            &spam_dir,
            Some(spambox_db_max_bytes),
        ) {
            Ok(store) => Some(store),
            Err(err) => {
                tracing::warn!("Failed to open social graph spambox for crawler: {}", err);
                None
            }
        }
    } else {
        None
    };
    let crawler_spambox_backend = crawler_spambox
        .clone()
        .map(|store| store as Arc<dyn socialgraph::SocialGraphBackend>);

    let background_services_controller = Arc::new(EmbeddedBackgroundServicesController::new(
        keys.clone(),
        opts.data_dir.clone(),
        Arc::clone(&store),
        graph_store.clone(),
        Arc::clone(&social_graph_store),
        crawler_spambox_backend,
    ));

    let upstream_blossom = config.blossom.upstream_read_servers(&opts.bind_address);
    let blossom_replica_queue_bytes = crate::server::bounded_upload_queue_bytes(
        config
            .blossom
            .replicate_queue_mb
            .saturating_mul(1024 * 1024),
    );
    let active_nostr_relays = config.nostr.active_relays();
    let fips_handle = crate::fips_transport::start_daemon_fips_transport(
        &config,
        &keys,
        Arc::clone(&store),
        fips_peer_ids,
    )
    .await?
    .map(Arc::new);
    let nostr_cache = crate::fips_transport::new_daemon_nostr_cache(store.store_arc());
    let nostr_provider = crate::fips_transport::start_daemon_nostr_provider(
        &config,
        fips_handle.as_deref(),
        Some(Arc::clone(&nostr_cache)),
    )
    .await?;
    #[cfg(feature = "experimental-decentralized-pubsub")]
    let nostr_pubsub_handle = crate::fips_transport::start_daemon_nostr_pubsub(
        &config,
        fips_handle.as_deref(),
        nostr_relay.clone(),
        nostr_cache,
    )
    .await?;

    let mut server = HashtreeServer::new(Arc::clone(&store), opts.bind_address.clone())
        .with_server_mode(config.server.mode)
        .with_hash_get_enabled(config.server.mode.hash_get_enabled())
        .with_fetch_from_fips_peers(config.server.fetch_from_fips_peers)
        .with_allowed_pubkeys(allowed_pubkeys.clone())
        .with_max_upload_bytes((config.blossom.max_upload_mb as usize) * 1024 * 1024)
        .with_public_writes(config.server.public_writes)
        .with_public_plaintext_reads(config.server.public_plaintext_reads)
        .with_require_random_untrusted_ingest(config.blossom.require_random_untrusted_ingest)
        .with_optimistic_blossom_uploads(config.blossom.optimistic_uploads)
        .with_upstream_blossom(upstream_blossom)
        .with_blossom_upload_replicas(
            config.blossom.replicate_servers.clone(),
            blossom_replica_queue_bytes,
            keys.clone(),
        )
        .with_nostr_relay_urls(active_nostr_relays)
        .with_cached_tree_roots(opts.initial_tree_roots)
        .with_social_graph(social_graph)
        .with_socialgraph_snapshot(
            Arc::clone(&social_graph_store),
            social_graph_root_bytes,
            config.server.socialgraph_snapshot_public,
        );
    if let Some(nostr_relay) = nostr_relay {
        server = server.with_nostr_relay(nostr_relay);
    }
    if let Some(provider) = nostr_provider {
        server = server.with_nostr_provider(provider);
    }

    if let Some(ref fips_handle) = fips_handle {
        server = server
            .with_fips_endpoint(fips_handle.endpoint.clone())
            .with_fips_blob_resolver(fips_handle.blob_resolver.clone());
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
    background_services_controller.apply_config(&config).await?;
    let daemon_controller = Arc::new(EmbeddedDaemonController::new(
        server_controller,
        fips_handle.clone(),
        #[cfg(feature = "experimental-decentralized-pubsub")]
        nostr_pubsub_handle.clone(),
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
        background_services_controller: Some(background_services_controller),
    })
}

fn embedded_nostr_enabled_after_relay_override(config: &Config) -> bool {
    config.nostr.decentralized_pubsub || !config.nostr.relays.is_empty()
}

#[cfg(test)]
mod tests {
    use super::{
        embedded_nostr_enabled_after_relay_override, EmbeddedBackgroundServicesController,
    };
    use crate::config::Config;

    #[test]
    fn mirror_publish_relays_orders_known_root_publish_relays_first() {
        let relays = EmbeddedBackgroundServicesController::mirror_publish_relays(
            &[
                "wss://graph-relay.iris.to".to_string(),
                "wss://relay.example".to_string(),
                "wss://relay.primal.net".to_string(),
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
                "wss://relay.example".to_string(),
                "wss://relay.primal.net".to_string(),
            ]
        );
    }

    #[test]
    fn mirror_publish_relays_do_not_add_non_active_publish_targets() {
        let relays = EmbeddedBackgroundServicesController::mirror_publish_relays(
            &[
                "wss://graph-relay.iris.to".to_string(),
                "wss://relay.example".to_string(),
            ],
            "0.0.0.0:8080",
        );
        assert_eq!(relays, vec!["wss://relay.example".to_string()]);
    }

    #[test]
    fn mirror_publish_relays_falls_back_to_active_relays_when_all_are_blocklisted() {
        let relays = EmbeddedBackgroundServicesController::mirror_publish_relays(
            &[
                "wss://graph-relay.iris.to".to_string(),
                "wss://upload.iris.to/nostr".to_string(),
            ],
            "0.0.0.0:8080",
        );
        assert_eq!(
            relays,
            vec![
                "wss://graph-relay.iris.to".to_string(),
                "wss://upload.iris.to/nostr".to_string(),
            ]
        );
    }

    #[test]
    fn nostr_mirror_config_maps_legacy_and_complete_archive_settings() {
        let mut config = Config::default();
        config.nostr.full_text_note_history_max_relay_pages = 0;
        config.nostr.archive_history_follow_distance = None;
        config.nostr.archive_history_max_relay_pages = 0;

        let mirror_config = EmbeddedBackgroundServicesController::nostr_mirror_config(
            &config,
            &["wss://relay.example".to_string()],
        );

        assert_eq!(mirror_config.full_text_note_history_max_relay_pages, 0);
        assert_eq!(mirror_config.archive_history_follow_distance, None);
        assert_eq!(mirror_config.archive_history_max_relay_pages, 0);

        config.nostr.full_text_note_history_max_relay_pages = 64;
        config.nostr.archive_history_follow_distance = Some(1);
        config.nostr.archive_history_max_relay_pages = 32;
        let mirror_config = EmbeddedBackgroundServicesController::nostr_mirror_config(
            &config,
            &["wss://relay.example".to_string()],
        );

        assert_eq!(mirror_config.full_text_note_history_max_relay_pages, 64);
        assert_eq!(mirror_config.archive_history_follow_distance, Some(1));
        assert_eq!(mirror_config.archive_history_max_relay_pages, 32);
    }

    #[test]
    fn nostr_mirror_config_can_limit_mirror_distance_independently() {
        let mut config = Config::default();
        config.nostr.social_graph_crawl_depth = 6;
        config.nostr.mirror_max_follow_distance = Some(2);

        let mirror_config = EmbeddedBackgroundServicesController::nostr_mirror_config(
            &config,
            &["wss://relay.example".to_string()],
        );

        assert_eq!(mirror_config.max_follow_distance, 2);

        config.nostr.mirror_max_follow_distance = None;
        let mirror_config = EmbeddedBackgroundServicesController::nostr_mirror_config(
            &config,
            &["wss://relay.example".to_string()],
        );

        assert_eq!(mirror_config.max_follow_distance, 6);
    }

    #[test]
    fn embedded_empty_relays_keep_nostr_enabled_for_decentralized_pubsub() {
        let mut config = Config::default();
        config.nostr.relays = Vec::new();
        config.nostr.decentralized_pubsub = false;
        assert!(!embedded_nostr_enabled_after_relay_override(&config));

        config.nostr.decentralized_pubsub = true;
        assert!(embedded_nostr_enabled_after_relay_override(&config));

        config.nostr.decentralized_pubsub = false;
        config.nostr.relays = vec!["wss://relay.example".to_string()];
        assert!(embedded_nostr_enabled_after_relay_override(&config));
    }
}
