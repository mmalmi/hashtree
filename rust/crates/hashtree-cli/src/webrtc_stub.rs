//! Stub module for when P2P feature is disabled
//! Provides minimal types to allow code to compile without webrtc dependencies

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Connection state stub
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
    Disconnected,
}

/// Peer entry stub
#[derive(Debug)]
pub struct PeerEntry {
    pub state: ConnectionState,
    pub peer_id: PeerId,
    pub peer: Option<DummyPeer>,
    pub pool: PeerPool,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

/// Peer ID stub
#[derive(Debug)]
pub struct PeerId {
    pub pubkey: String,
}

impl PeerId {
    pub fn short(&self) -> &str {
        &self.pubkey[..8.min(self.pubkey.len())]
    }
}

/// Dummy peer stub
#[derive(Debug)]
pub struct DummyPeer;

impl DummyPeer {
    pub fn has_data_channel(&self) -> bool { false }
    pub fn state(&self) -> &str { "Disabled" }
    pub async fn request(&self, _hash: &str) -> Result<Option<Vec<u8>>> { Ok(None) }
}

/// Peer pool stub
#[derive(Debug, Clone, Copy)]
pub enum PeerPool {
    None,
}

/// WebRTC state stub - always empty when P2P is disabled
#[derive(Debug)]
pub struct WebRTCState {
    pub peers: Arc<RwLock<HashMap<String, PeerEntry>>>,
}

impl Default for WebRTCState {
    fn default() -> Self {
        Self {
            peers: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl WebRTCState {
    /// Query peers for data - always returns None when P2P is disabled
    pub async fn query_peers_for_data(&self, _hash: &str) -> Option<Vec<u8>> {
        None
    }

    /// Request from peers - always returns None when P2P is disabled
    pub async fn request_from_peers(&self, _hash: &str) -> Option<Vec<u8>> {
        None
    }

    /// Get bandwidth stats - always returns zeros when P2P is disabled
    pub fn get_bandwidth(&self) -> (u64, u64) {
        (0, 0)
    }
}

/// Content store trait stub
pub trait ContentStore: Send + Sync + 'static {
    /// Get content by hex hash
    fn get(&self, hash_hex: &str) -> Result<Option<Vec<u8>>>;
}
