//! Default production mesh store wrapper.
//!
//! This is intentionally a thin composition layer around the shared routed
//! mesh store core. The reusable routing/retrieval behavior lives in
//! [`crate::mesh_store_core`]; this wrapper only plugs in the default production
//! transport pair:
//! - signaling: Nostr websocket relays
//! - direct links: real WebRTC data channels

use crate::types::{MeshStats, MeshStoreConfig, PeerId};
use crate::{
    MeshReadSource, MeshRouter, MeshRoutingConfig, NostrRelayTransport, ProductionMeshStore,
    SelectorSummary, SignalingTransport, WebRtcPeerLinkFactory,
};
use async_trait::async_trait;
use hashtree_blossom::BlossomClient;
use hashtree_core::{Hash, Store, StoreError};
use nostr_sdk::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::Duration;

struct BlossomReadSource {
    id: String,
    client: BlossomClient,
}

#[async_trait]
impl MeshReadSource for BlossomReadSource {
    fn id(&self) -> &str {
        &self.id
    }

    async fn get(&self, hash: &Hash) -> Option<Vec<u8>> {
        self.client.download(&hex::encode(hash)).await.ok()
    }
}

#[derive(Debug, Error)]
pub enum MeshStoreError {
    #[error("Mesh transport error: {0}")]
    Transport(String),
    #[error("No peers available")]
    NoPeers,
    #[error("Data not found")]
    NotFound,
    #[error("Store error: {0}")]
    Store(#[from] StoreError),
}

struct LiveMesh<S: Store + Send + Sync + 'static> {
    transport: Arc<NostrRelayTransport>,
    store: Arc<ProductionMeshStore<S>>,
    tasks: Vec<JoinHandle<()>>,
}

/// Production mesh-backed store that uses the shared routed mesh core.
pub struct MeshStore<S: Store + Send + Sync + 'static> {
    local_store: Arc<S>,
    config: MeshStoreConfig,
    live: Mutex<Option<LiveMesh<S>>>,
    running: Arc<AtomicBool>,
    stats_requests_made: AtomicU64,
    stats_requests_fulfilled: AtomicU64,
    stats_bytes_received: AtomicU64,
}

impl<S: Store + Send + Sync + 'static> MeshStore<S> {
    /// Create a new production mesh store wrapper.
    pub fn new(local_store: Arc<S>, config: MeshStoreConfig) -> Self {
        Self {
            local_store,
            config,
            live: Mutex::new(None),
            running: Arc::new(AtomicBool::new(false)),
            stats_requests_made: AtomicU64::new(0),
            stats_requests_fulfilled: AtomicU64::new(0),
            stats_bytes_received: AtomicU64::new(0),
        }
    }

    fn routing_config(&self) -> MeshRoutingConfig {
        MeshRoutingConfig {
            selection_strategy: self.config.request_selection_strategy,
            fairness_enabled: self.config.request_fairness_enabled,
            cashu_payment_weight: 0.0,
            cashu_payment_default_block_threshold: 0,
            cashu_accepted_mints: Vec::new(),
            cashu_default_mint: None,
            cashu_peer_suggested_mint_base_cap_sat: 0,
            cashu_peer_suggested_mint_success_step_sat: 0,
            cashu_peer_suggested_mint_receipt_step_sat: 0,
            cashu_peer_suggested_mint_max_cap_sat: 0,
            dispatch: self.config.request_dispatch,
            response_behavior: Default::default(),
            pubsub_scheduler: Default::default(),
        }
    }

    async fn live_store(&self) -> Option<Arc<ProductionMeshStore<S>>> {
        self.live
            .lock()
            .await
            .as_ref()
            .map(|live| live.store.clone())
    }

    /// Start the production transport composition and shared mesh core.
    pub async fn start(&mut self, keys: Keys) -> Result<(), MeshStoreError> {
        if self.running.load(Ordering::Relaxed) {
            return Ok(());
        }

        let peer_id = PeerId::new(keys.public_key().to_hex()).to_string();
        let transport = Arc::new(NostrRelayTransport::new(keys.clone(), self.config.debug));
        transport
            .connect(&self.config.relays)
            .await
            .map_err(|e| MeshStoreError::Transport(e.to_string()))?;

        let factory = Arc::new(WebRtcPeerLinkFactory::default());
        let mut router = MeshRouter::new(
            peer_id.clone(),
            transport.clone(),
            factory,
            self.config.pools.clone(),
            self.config.debug,
        );
        if let Some(classifier_tx) = self.config.classifier_tx.clone() {
            router.set_classifier(classifier_tx);
        }

        let store = Arc::new(ProductionMeshStore::new_with_routing(
            self.local_store.clone(),
            Arc::new(router),
            Duration::from_millis(self.config.request_timeout_ms),
            self.config.debug,
            self.routing_config(),
        ));
        if !self.config.upstream_blossom_servers.is_empty() {
            let timeout = Duration::from_millis(self.config.request_timeout_ms);
            let sources: Vec<Arc<dyn MeshReadSource>> = self
                .config
                .upstream_blossom_servers
                .iter()
                .map(|server| {
                    Arc::new(BlossomReadSource {
                        id: format!("blossom:{server}"),
                        client: BlossomClient::new_empty(keys.clone())
                            .with_timeout(timeout)
                            .with_read_servers(vec![server.clone()]),
                    }) as Arc<dyn MeshReadSource>
                })
                .collect();
            store.set_read_sources(sources).await;
        }
        store
            .start()
            .await
            .map_err(|e| MeshStoreError::Transport(e.to_string()))?;

        self.running.store(true, Ordering::Relaxed);

        let running = self.running.clone();
        let signaling_store = store.clone();
        let signaling_transport = transport.clone();
        let signaling_task = tokio::spawn(async move {
            loop {
                if !running.load(Ordering::Relaxed) {
                    break;
                }

                let Some(msg) = signaling_transport.recv().await else {
                    break;
                };
                if signaling_store.process_signaling(msg).await.is_err() {
                    continue;
                }
            }
        });

        let running = self.running.clone();
        let data_store = store.clone();
        let data_task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(10));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                let _ = data_store.drain_available_data_messages().await;
            }
        });

        let running = self.running.clone();
        let hello_store = store.clone();
        let hello_interval_ms = self.config.hello_interval_ms;
        let hello_task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(hello_interval_ms));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                let _ = hello_store.send_hello().await;
            }
        });

        *self.live.lock().await = Some(LiveMesh {
            transport,
            store,
            tasks: vec![signaling_task, data_task, hello_task],
        });

        Ok(())
    }

    /// Stop the production transport composition and background tasks.
    pub async fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);

        let live = self.live.lock().await.take();
        let Some(live) = live else {
            return;
        };

        live.store.stop().await;

        let peer_ids = live.store.signaling().peer_ids().await;
        for peer_id in peer_ids {
            if let Some(channel) = live.store.signaling().get_channel(&peer_id).await {
                channel.close().await;
            }
        }

        for task in live.tasks {
            task.abort();
        }
        live.transport.disconnect().await;
    }

    /// Snapshot high-level store statistics.
    pub async fn stats(&self) -> MeshStats {
        MeshStats {
            connected_peers: self.peer_count().await,
            pending_requests: 0,
            bytes_sent: 0,
            bytes_received: self.stats_bytes_received.load(Ordering::Relaxed),
            requests_made: self.stats_requests_made.load(Ordering::Relaxed),
            requests_fulfilled: self.stats_requests_fulfilled.load(Ordering::Relaxed),
        }
    }

    /// Get the number of currently connected peers.
    pub async fn peer_count(&self) -> usize {
        let Some(store) = self.live_store().await else {
            return 0;
        };
        store.peer_count().await
    }

    /// Get adaptive routing selector summary statistics.
    pub async fn selector_summary(&self) -> SelectorSummary {
        let Some(store) = self.live_store().await else {
            return SelectorSummary::default();
        };
        store.selector_summary().await
    }
}

#[async_trait]
impl<S: Store + Send + Sync + 'static> Store for MeshStore<S> {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.local_store.put(hash, data).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        if let Some(data) = self.local_store.get(hash).await? {
            return Ok(Some(data));
        }

        self.stats_requests_made.fetch_add(1, Ordering::Relaxed);

        let Some(store) = self.live_store().await else {
            return Ok(None);
        };

        let result = store.get(hash).await?;
        if let Some(ref data) = result {
            self.stats_requests_fulfilled
                .fetch_add(1, Ordering::Relaxed);
            self.stats_bytes_received
                .fetch_add(data.len() as u64, Ordering::Relaxed);
        }
        Ok(result)
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.local_store.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.local_store.delete(hash).await
    }
}
