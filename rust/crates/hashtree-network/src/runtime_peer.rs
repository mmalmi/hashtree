use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;

use crate::mesh_session::MeshSession;
use crate::types::{PeerId, PeerPool, PoolSettings};

pub type PeerClassifier = Arc<dyn Fn(&str) -> PeerPool + Send + Sync>;

/// Active data transport used for a peer session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PeerTransport {
    Direct,
    Bluetooth,
}

impl PeerTransport {
    pub const fn as_str(self) -> &'static str {
        match self {
            PeerTransport::Direct => "direct",
            PeerTransport::Bluetooth => "bluetooth",
        }
    }
}

impl std::fmt::Display for PeerTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str((*self).as_str())
    }
}

/// Signaling/discovery path through which a peer was seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PeerSignalPath {
    Relay,
    Multicast,
    WifiAware,
    Bluetooth,
}

impl PeerSignalPath {
    pub const fn as_str(self) -> &'static str {
        match self {
            PeerSignalPath::Relay => "relay",
            PeerSignalPath::Multicast => "multicast",
            PeerSignalPath::WifiAware => "wifi-aware",
            PeerSignalPath::Bluetooth => "bluetooth",
        }
    }

    pub fn from_source_name(source: &str) -> Self {
        match source {
            "multicast" => PeerSignalPath::Multicast,
            "wifi-aware" => PeerSignalPath::WifiAware,
            "bluetooth" => PeerSignalPath::Bluetooth,
            _ => PeerSignalPath::Relay,
        }
    }
}

impl std::fmt::Display for PeerSignalPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str((*self).as_str())
    }
}

/// Direction of peer connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerDirection {
    Inbound,
    Outbound,
}

impl std::fmt::Display for PeerDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerDirection::Inbound => write!(f, "inbound"),
            PeerDirection::Outbound => write!(f, "outbound"),
        }
    }
}

/// Connection state for a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Discovered,
    Connecting,
    Connected,
    Failed,
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionState::Discovered => write!(f, "discovered"),
            ConnectionState::Connecting => write!(f, "connecting"),
            ConnectionState::Connected => write!(f, "connected"),
            ConnectionState::Failed => write!(f, "failed"),
        }
    }
}

/// Shared peer entry for transport-backed runtime peers.
pub struct MeshPeerEntry<P> {
    pub peer_id: PeerId,
    pub direction: PeerDirection,
    pub state: ConnectionState,
    pub last_seen: Instant,
    pub peer: Option<P>,
    pub pool: PeerPool,
    pub transport: PeerTransport,
    pub signal_paths: BTreeSet<PeerSignalPath>,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

pub async fn remember_peer_signal_path<P>(
    peers: &RwLock<HashMap<String, MeshPeerEntry<P>>>,
    peer_id: &str,
    source: &str,
) {
    if let Some(entry) = peers.write().await.get_mut(peer_id) {
        entry
            .signal_paths
            .insert(PeerSignalPath::from_source_name(source));
    }
}

pub fn can_track_signal_path_peer<P>(
    signal_path: PeerSignalPath,
    max_peers: usize,
    peer_key: &str,
    peers: &HashMap<String, MeshPeerEntry<P>>,
) -> bool {
    if peers.contains_key(peer_key) {
        return true;
    }
    if max_peers == 0 {
        return false;
    }
    peers
        .values()
        .filter(|entry| {
            entry.signal_paths.contains(&signal_path) && entry.state != ConnectionState::Failed
        })
        .count()
        < max_peers
}

/// Shared registrar for transport-native direct peer sessions.
#[derive(Clone)]
pub struct TransportPeerRegistrar<P> {
    peers: Arc<RwLock<HashMap<String, MeshPeerEntry<P>>>>,
    connected_count: Arc<AtomicUsize>,
    peer_classifier: PeerClassifier,
    pools: PoolSettings,
    transport: PeerTransport,
    signal_path: PeerSignalPath,
    max_transport_peers: usize,
}

impl<P> TransportPeerRegistrar<P>
where
    P: MeshSession + Send + Sync + 'static,
{
    pub fn new(
        peers: Arc<RwLock<HashMap<String, MeshPeerEntry<P>>>>,
        connected_count: Arc<AtomicUsize>,
        peer_classifier: PeerClassifier,
        pools: PoolSettings,
        transport: PeerTransport,
        signal_path: PeerSignalPath,
        max_transport_peers: usize,
    ) -> Self {
        Self {
            peers,
            connected_count,
            peer_classifier,
            pools,
            transport,
            signal_path,
            max_transport_peers,
        }
    }

    async fn pool_counts(&self) -> (usize, usize) {
        let peers = self.peers.read().await;
        let mut follows = 0usize;
        let mut other = 0usize;
        for entry in peers.values() {
            if entry.state != ConnectionState::Connected {
                continue;
            }
            match entry.pool {
                PeerPool::Follows => follows += 1,
                PeerPool::Other => other += 1,
            }
        }
        (follows, other)
    }

    async fn transport_peer_count(&self, peer_key: &str) -> usize {
        let peers = self.peers.read().await;
        peers
            .values()
            .filter(|entry| entry.transport == self.transport)
            .filter(|entry| entry.state == ConnectionState::Connected)
            .filter(|entry| entry.peer_id.to_string() != peer_key)
            .count()
    }

    pub async fn register_connected_peer(
        &self,
        peer_id: PeerId,
        direction: PeerDirection,
        peer: P,
    ) -> bool {
        let peer_key = peer_id.to_string();
        let pool = (self.peer_classifier)(&peer_id.pubkey);
        let (follows, other) = self.pool_counts().await;
        let can_accept_pool = match pool {
            PeerPool::Follows => follows < self.pools.follows.max_connections,
            PeerPool::Other => other < self.pools.other.max_connections,
        };
        if !can_accept_pool {
            return false;
        }

        if self.max_transport_peers == 0
            || self.transport_peer_count(&peer_key).await >= self.max_transport_peers
        {
            return false;
        }

        let mut peers = self.peers.write().await;
        let duplicate_keys = peers
            .iter()
            .filter(|(key, entry)| {
                key.as_str() != peer_key
                    && entry.transport == self.transport
                    && entry.peer_id.pubkey == peer_id.pubkey
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        let was_connected = peers
            .get(&peer_key)
            .map(|entry| entry.state == ConnectionState::Connected)
            .unwrap_or(false);
        let replaced = peers.insert(
            peer_key,
            MeshPeerEntry {
                peer_id,
                direction,
                state: ConnectionState::Connected,
                last_seen: Instant::now(),
                peer: Some(peer),
                pool,
                transport: self.transport,
                signal_paths: BTreeSet::from([self.signal_path]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );
        let removed_duplicates = duplicate_keys
            .into_iter()
            .filter_map(|key| peers.remove(&key))
            .collect::<Vec<_>>();
        drop(peers);

        if let Some(previous) = replaced.and_then(|entry| entry.peer) {
            let _ = previous.close().await;
        }
        for duplicate in &removed_duplicates {
            if let Some(peer) = duplicate.peer.as_ref() {
                let _ = peer.close().await;
            }
        }

        let removed_connected_duplicates = removed_duplicates
            .iter()
            .filter(|entry| entry.state == ConnectionState::Connected)
            .count() as isize;
        let connected_delta =
            1isize - if was_connected { 1 } else { 0 } - removed_connected_duplicates;
        if connected_delta > 0 {
            self.connected_count
                .fetch_add(connected_delta as usize, Ordering::Relaxed);
        } else if connected_delta < 0 {
            self.connected_count
                .fetch_sub((-connected_delta) as usize, Ordering::Relaxed);
        }
        true
    }

    pub async fn unregister_peer(&self, peer_id: &PeerId) {
        let peer_key = peer_id.to_string();
        let removed = self.peers.write().await.remove(&peer_key);
        self.finish_unregister(removed).await;
    }

    pub async fn unregister_peer_if<F>(&self, peer_id: &PeerId, predicate: F)
    where
        F: FnOnce(&P) -> bool + Send,
    {
        let peer_key = peer_id.to_string();
        let removed = {
            let mut peers = self.peers.write().await;
            let matches_current = peers
                .get(&peer_key)
                .and_then(|entry| entry.peer.as_ref())
                .map(predicate)
                .unwrap_or(false);
            if matches_current {
                peers.remove(&peer_key)
            } else {
                None
            }
        };
        self.finish_unregister(removed).await;
    }

    async fn finish_unregister(&self, removed: Option<MeshPeerEntry<P>>) {
        if let Some(entry) = removed {
            if entry.state == ConnectionState::Connected {
                self.connected_count.fetch_sub(1, Ordering::Relaxed);
            }
            if let Some(peer) = entry.peer {
                let _ = peer.close().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use nostr_sdk::nostr::{Event, Filter};
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use std::time::Duration;

    use crate::types::{MeshNostrFrame, PeerHTLConfig, PoolConfig};

    struct TestSession {
        closed: AtomicBool,
    }

    impl TestSession {
        fn new() -> Self {
            Self {
                closed: AtomicBool::new(false),
            }
        }

        fn is_closed(&self) -> bool {
            self.closed.load(AtomicOrdering::Relaxed)
        }
    }

    #[async_trait]
    impl MeshSession for Arc<TestSession> {
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

    fn test_pools() -> PoolSettings {
        PoolSettings {
            follows: PoolConfig {
                max_connections: 4,
                satisfied_connections: 0,
            },
            other: PoolConfig {
                max_connections: 4,
                satisfied_connections: 0,
            },
        }
    }

    fn test_registrar() -> (
        TransportPeerRegistrar<Arc<TestSession>>,
        Arc<RwLock<HashMap<String, MeshPeerEntry<Arc<TestSession>>>>>,
        Arc<AtomicUsize>,
    ) {
        let peers = Arc::new(RwLock::new(HashMap::new()));
        let connected_count = Arc::new(AtomicUsize::new(0));
        let registrar = TransportPeerRegistrar::new(
            peers.clone(),
            connected_count.clone(),
            Arc::new(|_| PeerPool::Other),
            test_pools(),
            PeerTransport::Bluetooth,
            PeerSignalPath::Bluetooth,
            2,
        );
        (registrar, peers, connected_count)
    }

    #[tokio::test]
    async fn register_connected_peer_closes_replaced_session() {
        let (registrar, _peers, _connected_count) = test_registrar();
        let peer_id = PeerId::new("peer-pub".to_string());
        let first = Arc::new(TestSession::new());
        let second = Arc::new(TestSession::new());

        assert!(
            registrar
                .register_connected_peer(peer_id.clone(), PeerDirection::Outbound, first.clone())
                .await
        );
        assert!(
            registrar
                .register_connected_peer(peer_id, PeerDirection::Outbound, second)
                .await
        );

        assert!(first.is_closed());
    }

    #[tokio::test]
    async fn register_connected_peer_replaces_existing_transport_session_for_same_pubkey() {
        let (registrar, peers, connected_count) = test_registrar();
        let first_peer_id = PeerId::new("peer-pub".to_string());
        let second_peer_id = PeerId::new("peer-pub".to_string());
        let first = Arc::new(TestSession::new());
        let second = Arc::new(TestSession::new());

        assert!(
            registrar
                .register_connected_peer(
                    first_peer_id.clone(),
                    PeerDirection::Outbound,
                    first.clone(),
                )
                .await
        );
        assert!(
            registrar
                .register_connected_peer(second_peer_id.clone(), PeerDirection::Outbound, second,)
                .await
        );

        assert!(first.is_closed());
        let peers = peers.read().await;
        assert!(peers.contains_key(&second_peer_id.to_string()));
        assert_eq!(peers.len(), 1);
        assert_eq!(connected_count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn unregister_peer_if_respects_current_predicate() {
        let (registrar, peers, connected_count) = test_registrar();
        let peer_id = PeerId::new("peer-pub".to_string());
        let session = Arc::new(TestSession::new());

        assert!(
            registrar
                .register_connected_peer(peer_id.clone(), PeerDirection::Outbound, session.clone(),)
                .await
        );
        registrar
            .unregister_peer_if(&peer_id, |current| Arc::ptr_eq(current, &session))
            .await;

        assert!(session.is_closed());
        assert!(!peers.read().await.contains_key(&peer_id.to_string()));
        assert_eq!(connected_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn can_track_signal_path_peer_enforces_limit() {
        let existing_peer = PeerId::new("peer-a".to_string());
        let existing_key = existing_peer.to_string();
        let mut peers = HashMap::new();
        peers.insert(
            existing_key.clone(),
            MeshPeerEntry::<Arc<TestSession>> {
                peer_id: existing_peer,
                direction: PeerDirection::Outbound,
                state: ConnectionState::Discovered,
                last_seen: Instant::now(),
                peer: None,
                pool: PeerPool::Other,
                transport: PeerTransport::Direct,
                signal_paths: BTreeSet::from([PeerSignalPath::WifiAware]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );

        assert!(can_track_signal_path_peer(
            PeerSignalPath::WifiAware,
            1,
            &existing_key,
            &peers
        ));
        assert!(!can_track_signal_path_peer(
            PeerSignalPath::WifiAware,
            1,
            "peer-b",
            &peers
        ));
        assert!(can_track_signal_path_peer(
            PeerSignalPath::Relay,
            1,
            "peer-c",
            &peers
        ));
    }
}
