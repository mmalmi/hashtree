use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::RwLock;

use crate::local_bus::SharedLocalNostrBus;
use crate::mesh_session::{resolve_root_from_local_buses_with_source, MeshSession};
use crate::root_events::PeerRootEvent;
use crate::runtime_peer::MeshPeerEntry;
use crate::types::{KnownPeerRecord, KnownPeerSnapshot};

/// Shared runtime state for transport-backed mesh peers.
pub struct MeshRuntimeState<P> {
    pub peers: Arc<RwLock<HashMap<String, MeshPeerEntry<P>>>>,
    pub connected_count: Arc<AtomicUsize>,
    peer_hash_get: Arc<RwLock<HashMap<String, bool>>>,
    pub bytes_sent: AtomicU64,
    pub bytes_received: AtomicU64,
    pub mesh_received: AtomicU64,
    pub mesh_forwarded: AtomicU64,
    pub mesh_dropped_duplicate: AtomicU64,
    local_buses: RwLock<Vec<SharedLocalNostrBus>>,
    known_peers: RwLock<HashMap<String, KnownPeerRecord>>,
}

impl<P> Default for MeshRuntimeState<P>
where
    P: MeshSession + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<P> MeshRuntimeState<P>
where
    P: MeshSession + Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self {
            peers: Arc::new(RwLock::new(HashMap::new())),
            connected_count: Arc::new(AtomicUsize::new(0)),
            peer_hash_get: Arc::new(RwLock::new(HashMap::new())),
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            mesh_received: AtomicU64::new(0),
            mesh_forwarded: AtomicU64::new(0),
            mesh_dropped_duplicate: AtomicU64::new(0),
            local_buses: RwLock::new(Vec::new()),
            known_peers: RwLock::new(HashMap::new()),
        }
    }

    pub async fn set_local_buses(&self, buses: Vec<SharedLocalNostrBus>) {
        *self.local_buses.write().await = buses;
    }

    pub async fn add_local_bus(&self, bus: SharedLocalNostrBus) {
        self.local_buses.write().await.push(bus);
    }

    pub async fn set_peer_hash_get(&self, peer_id: &str, enabled: bool) {
        self.peer_hash_get
            .write()
            .await
            .insert(peer_id.to_string(), enabled);
    }

    pub async fn clear_peer_hash_get(&self, peer_id: &str) {
        self.peer_hash_get.write().await.remove(peer_id);
    }

    pub async fn peer_hash_get_enabled(&self, peer_id: &str) -> bool {
        self.peer_hash_get
            .read()
            .await
            .get(peer_id)
            .copied()
            .unwrap_or(true)
    }

    pub async fn peer_hash_get_snapshot(&self) -> HashMap<String, bool> {
        self.peer_hash_get.read().await.clone()
    }

    pub async fn record_known_peer_signal_urls(
        &self,
        peer_id: &str,
        signal_urls: &[String],
        source: &str,
    ) {
        let clean_signal_urls = normalize_peer_signal_urls(signal_urls);
        if clean_signal_urls.is_empty() {
            return;
        }

        let mut known = self.known_peers.write().await;
        let entry = known
            .entry(peer_id.to_string())
            .or_insert_with(|| KnownPeerRecord {
                peer_id: peer_id.to_string(),
                signal_urls: Vec::new(),
                last_seen_unix_ms: 0,
                last_source: None,
            });
        for signal_url in clean_signal_urls {
            if !entry.signal_urls.contains(&signal_url) {
                entry.signal_urls.push(signal_url);
            }
        }
        entry.signal_urls.sort();
        entry.last_seen_unix_ms = now_unix_ms();
        entry.last_source = Some(source.to_string());
    }

    pub async fn known_peer_snapshot(&self) -> KnownPeerSnapshot {
        let mut peers: Vec<KnownPeerRecord> =
            self.known_peers.read().await.values().cloned().collect();
        peers.sort_by(|a, b| a.peer_id.cmp(&b.peer_id));
        KnownPeerSnapshot { version: 1, peers }
    }

    pub async fn import_known_peer_snapshot(&self, snapshot: &KnownPeerSnapshot) {
        if snapshot.version != 1 {
            return;
        }
        let mut known = self.known_peers.write().await;
        known.clear();
        for peer in &snapshot.peers {
            let signal_urls = normalize_peer_signal_urls(&peer.signal_urls);
            if peer.peer_id.trim().is_empty() || signal_urls.is_empty() {
                continue;
            }
            known.insert(
                peer.peer_id.clone(),
                KnownPeerRecord {
                    peer_id: peer.peer_id.clone(),
                    signal_urls,
                    last_seen_unix_ms: peer.last_seen_unix_ms,
                    last_source: peer.last_source.clone(),
                },
            );
        }
    }

    pub async fn local_buses(&self) -> Vec<SharedLocalNostrBus> {
        self.local_buses.read().await.clone()
    }

    pub async fn reset(&self) {
        self.set_local_buses(Vec::new()).await;
        let peers = {
            let mut peers = self.peers.write().await;
            std::mem::take(&mut *peers)
        };
        self.peer_hash_get.write().await.clear();
        self.connected_count.store(0, Ordering::Relaxed);
        for entry in peers.into_values() {
            if let Some(peer) = entry.peer {
                let _ = peer.close().await;
            }
        }
    }

    pub fn get_bandwidth(&self) -> (u64, u64) {
        (
            self.bytes_sent.load(Ordering::Relaxed),
            self.bytes_received.load(Ordering::Relaxed),
        )
    }

    pub fn get_mesh_stats(&self) -> (u64, u64, u64) {
        (
            self.mesh_received.load(Ordering::Relaxed),
            self.mesh_forwarded.load(Ordering::Relaxed),
            self.mesh_dropped_duplicate.load(Ordering::Relaxed),
        )
    }

    pub fn record_mesh_received(&self) {
        self.mesh_received.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_mesh_forwarded(&self, count: u64) {
        self.mesh_forwarded.fetch_add(count, Ordering::Relaxed);
    }

    pub fn record_mesh_duplicate_drop(&self) {
        self.mesh_dropped_duplicate.fetch_add(1, Ordering::Relaxed);
    }

    pub async fn record_sent(&self, peer_id: &str, bytes: u64) {
        self.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
        if let Some(entry) = self.peers.write().await.get_mut(peer_id) {
            entry.bytes_sent += bytes;
        }
    }

    pub async fn record_received(&self, peer_id: &str, bytes: u64) {
        self.bytes_received.fetch_add(bytes, Ordering::Relaxed);
        if let Some(entry) = self.peers.write().await.get_mut(peer_id) {
            entry.bytes_received += bytes;
        }
    }

    pub async fn resolve_root_from_local_buses_with_source(
        &self,
        owner_pubkey: &str,
        tree_name: &str,
        timeout: Duration,
    ) -> Option<(&'static str, PeerRootEvent)> {
        resolve_root_from_local_buses_with_source(
            self.local_buses().await,
            owner_pubkey,
            tree_name,
            timeout,
        )
        .await
    }

    pub async fn resolve_root_from_local_buses(
        &self,
        owner_pubkey: &str,
        tree_name: &str,
        timeout: Duration,
    ) -> Option<PeerRootEvent> {
        self.resolve_root_from_local_buses_with_source(owner_pubkey, tree_name, timeout)
            .await
            .map(|(_, root)| root)
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn normalize_peer_signal_urls(signal_urls: &[String]) -> Vec<String> {
    let mut output = Vec::new();
    for signal_url in signal_urls {
        let trimmed = signal_url.trim().trim_end_matches('/').to_string();
        if trimmed.is_empty() || !trimmed.starts_with("http://") || output.contains(&trimmed) {
            continue;
        }
        output.push(trimmed);
    }
    output.sort();
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use nostr_sdk::nostr::{Event, Filter};
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use std::time::Instant;

    use crate::local_bus::LocalNostrBus;
    use crate::runtime_peer::{
        ConnectionState, MeshPeerEntry, PeerDirection, PeerSignalPath, PeerTransport,
    };
    use crate::types::{MeshNostrFrame, PeerHTLConfig, PeerId, PeerPool};

    struct TestSession {
        closed: AtomicBool,
    }

    #[async_trait]
    impl MeshSession for TestSession {
        fn is_ready(&self) -> bool {
            true
        }

        fn is_connected(&self) -> bool {
            true
        }

        fn htl_config(&self) -> PeerHTLConfig {
            PeerHTLConfig::from_flags(false, false)
        }

        async fn request(&self, _hash_hex: &str, _timeout: Duration) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }

        async fn query_nostr_events(
            &self,
            _filters: Vec<Filter>,
            _timeout: Duration,
        ) -> Result<Vec<Event>> {
            Ok(Vec::new())
        }

        async fn send_mesh_frame_text(&self, _frame: &MeshNostrFrame) -> Result<()> {
            Ok(())
        }

        async fn close(&self) -> Result<()> {
            self.closed.store(true, AtomicOrdering::Relaxed);
            Ok(())
        }
    }

    struct TestLocalBus {
        source: &'static str,
        root: Option<PeerRootEvent>,
    }

    #[async_trait]
    impl LocalNostrBus for TestLocalBus {
        fn source_name(&self) -> &'static str {
            self.source
        }

        async fn broadcast_event(&self, _event: &Event) -> Result<()> {
            Ok(())
        }

        async fn query_root(
            &self,
            _owner_pubkey: &str,
            _tree_name: &str,
            _timeout: Duration,
        ) -> Option<PeerRootEvent> {
            self.root.clone()
        }
    }

    #[tokio::test]
    async fn record_updates_global_and_per_peer_counters() {
        let runtime = MeshRuntimeState::<TestSession>::new();
        let peer_id = PeerId::new("peer-a".to_string());
        let peer_key = peer_id.to_string();
        runtime.peers.write().await.insert(
            peer_key.clone(),
            MeshPeerEntry {
                peer_id,
                direction: PeerDirection::Outbound,
                state: ConnectionState::Connected,
                last_seen: Instant::now(),
                peer: None,
                pool: PeerPool::Other,
                transport: PeerTransport::WebRtc,
                signal_paths: BTreeSet::from([PeerSignalPath::Relay]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );

        runtime.record_sent(&peer_key, 16).await;
        runtime.record_received(&peer_key, 32).await;

        assert_eq!(runtime.get_bandwidth(), (16, 32));
        let peers = runtime.peers.read().await;
        let entry = peers.get(&peer_key).expect("peer");
        assert_eq!(entry.bytes_sent, 16);
        assert_eq!(entry.bytes_received, 32);
    }

    #[tokio::test]
    async fn reset_closes_peers_and_clears_local_buses() {
        let runtime = MeshRuntimeState::<TestSession>::new();
        let session = TestSession {
            closed: AtomicBool::new(false),
        };
        let peer_id = PeerId::new("peer-a".to_string());
        runtime.peers.write().await.insert(
            peer_id.to_string(),
            MeshPeerEntry {
                peer_id,
                direction: PeerDirection::Outbound,
                state: ConnectionState::Connected,
                last_seen: Instant::now(),
                peer: Some(session),
                pool: PeerPool::Other,
                transport: PeerTransport::Bluetooth,
                signal_paths: BTreeSet::from([PeerSignalPath::Bluetooth]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );
        runtime.connected_count.store(1, Ordering::Relaxed);
        runtime
            .set_local_buses(vec![Arc::new(TestLocalBus {
                source: "mock",
                root: None,
            }) as SharedLocalNostrBus])
            .await;

        runtime.reset().await;

        assert_eq!(runtime.connected_count.load(Ordering::Relaxed), 0);
        assert!(runtime.peers.read().await.is_empty());
        assert!(runtime.local_buses().await.is_empty());
    }

    #[tokio::test]
    async fn resolve_root_from_local_buses_returns_first_match() {
        let runtime = MeshRuntimeState::<TestSession>::new();
        let root = PeerRootEvent {
            hash: "ab".repeat(32),
            key: None,
            encrypted_key: None,
            self_encrypted_key: None,
            event_id: "event-1".to_string(),
            created_at: 1,
            peer_id: "bus-peer".to_string(),
        };
        runtime
            .set_local_buses(vec![
                Arc::new(TestLocalBus {
                    source: "empty",
                    root: None,
                }) as SharedLocalNostrBus,
                Arc::new(TestLocalBus {
                    source: "mock-bus",
                    root: Some(root.clone()),
                }) as SharedLocalNostrBus,
            ])
            .await;

        let resolved = runtime
            .resolve_root_from_local_buses_with_source("owner", "tree", Duration::from_millis(10))
            .await
            .expect("root");

        assert_eq!(resolved.0, "mock-bus");
        assert_eq!(resolved.1, root);
    }
}
