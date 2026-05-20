//! Hashtree blob exchange over FIPS endpoint bytes.
//!
//! FIPS owns peer discovery, signaling, and underlay transports. This crate
//! keeps the Hashtree side to verified blob request/response frames carried as
//! app-owned endpoint bytes.

use async_trait::async_trait;
use hashtree_core::{Hash, MemoryStore, Store, StoreError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Duration, timeout};

pub const DEFAULT_FIPS_DISCOVERY_SCOPE: &str = "hashtree-v1";
pub const DEFAULT_FIPS_REQUEST_TIMEOUT: Duration = Duration::from_millis(5_500);
pub const MAX_HTL: u8 = 10;

const MSG_TYPE_REQUEST: u8 = 0x00;
const MSG_TYPE_RESPONSE: u8 = 0x01;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FipsEndpointPacket {
    pub peer_id: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum FipsTransportError {
    #[error("endpoint send failed: {0}")]
    Send(String),
    #[error("wire decode failed: {0}")]
    Wire(String),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
}

#[async_trait]
pub trait FipsEndpointIo: Send + Sync {
    async fn send(&self, peer_id: &str, data: Vec<u8>) -> Result<(), FipsTransportError>;
    async fn recv(&self) -> Option<FipsEndpointPacket>;
    async fn peer_ids(&self) -> Vec<String> {
        Vec::new()
    }
    fn local_peer_id(&self) -> Option<String> {
        None
    }
}

#[async_trait]
impl FipsEndpointIo for fips_core::FipsEndpoint {
    async fn send(&self, peer_id: &str, data: Vec<u8>) -> Result<(), FipsTransportError> {
        self.send(peer_id.to_string(), data)
            .await
            .map_err(|err| FipsTransportError::Send(err.to_string()))
    }

    async fn recv(&self) -> Option<FipsEndpointPacket> {
        loop {
            let message = fips_core::FipsEndpoint::recv(self).await?;
            if let Some(peer_id) = message.source_npub {
                return Some(FipsEndpointPacket {
                    peer_id,
                    data: message.data,
                });
            }
        }
    }

    async fn peer_ids(&self) -> Vec<String> {
        match self.peers().await {
            Ok(peers) => peers.into_iter().map(|peer| peer.npub).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn local_peer_id(&self) -> Option<String> {
        Some(self.npub().to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DataRequest {
    #[serde(with = "serde_bytes")]
    h: Vec<u8>,
    #[serde(default = "default_htl", skip_serializing_if = "is_max_htl")]
    htl: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DataResponse {
    #[serde(with = "serde_bytes")]
    h: Vec<u8>,
    #[serde(with = "serde_bytes")]
    d: Vec<u8>,
}

enum Message {
    Request(DataRequest),
    Response(DataResponse),
}

fn default_htl() -> u8 {
    MAX_HTL
}

fn is_max_htl(htl: &u8) -> bool {
    *htl == MAX_HTL
}

fn hash_key(hash: &Hash) -> String {
    hex::encode(hash)
}

fn bytes_to_hash(bytes: &[u8]) -> Option<Hash> {
    if bytes.len() != 32 {
        return None;
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(bytes);
    Some(hash)
}

fn verify_hash(data: &[u8], hash: &Hash) -> bool {
    let digest = Sha256::digest(data);
    digest.as_slice() == hash
}

fn encode_request(hash: &Hash, htl: u8) -> Result<Vec<u8>, FipsTransportError> {
    let body = rmp_serde::to_vec_named(&DataRequest {
        h: hash.to_vec(),
        htl,
    })
    .map_err(|err| FipsTransportError::Wire(err.to_string()))?;
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(MSG_TYPE_REQUEST);
    out.extend(body);
    Ok(out)
}

fn encode_response(hash: &Hash, data: &[u8]) -> Result<Vec<u8>, FipsTransportError> {
    let body = rmp_serde::to_vec_named(&DataResponse {
        h: hash.to_vec(),
        d: data.to_vec(),
    })
    .map_err(|err| FipsTransportError::Wire(err.to_string()))?;
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(MSG_TYPE_RESPONSE);
    out.extend(body);
    Ok(out)
}

fn parse_message(data: &[u8]) -> Result<Option<Message>, FipsTransportError> {
    let Some((&kind, body)) = data.split_first() else {
        return Ok(None);
    };
    match kind {
        MSG_TYPE_REQUEST => rmp_serde::from_slice::<DataRequest>(body)
            .map(|req| Some(Message::Request(req)))
            .map_err(|err| FipsTransportError::Wire(err.to_string())),
        MSG_TYPE_RESPONSE => rmp_serde::from_slice::<DataResponse>(body)
            .map(|resp| Some(Message::Response(resp)))
            .map_err(|err| FipsTransportError::Wire(err.to_string())),
        _ => Ok(None),
    }
}

struct PendingRequest {
    resolve: oneshot::Sender<Option<Vec<u8>>>,
}

pub struct HashtreeFipsTransport<S: Store + Send + Sync + 'static = MemoryStore> {
    endpoint: Arc<dyn FipsEndpointIo>,
    local_store: Arc<S>,
    peers: Arc<RwLock<Vec<String>>>,
    pending: Arc<Mutex<HashMap<String, Vec<PendingRequest>>>>,
    request_timeout: Duration,
    request_htl: u8,
    cache_responses: bool,
}

impl HashtreeFipsTransport<MemoryStore> {
    pub fn in_memory(endpoint: Arc<dyn FipsEndpointIo>) -> Self {
        Self::new(endpoint, Arc::new(MemoryStore::new()))
    }
}

impl<S: Store + Send + Sync + 'static> HashtreeFipsTransport<S> {
    pub fn new(endpoint: Arc<dyn FipsEndpointIo>, local_store: Arc<S>) -> Self {
        Self {
            endpoint,
            local_store,
            peers: Arc::new(RwLock::new(Vec::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            request_timeout: DEFAULT_FIPS_REQUEST_TIMEOUT,
            request_htl: MAX_HTL,
            cache_responses: true,
        }
    }

    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub fn with_request_htl(mut self, htl: u8) -> Self {
        self.request_htl = htl;
        self
    }

    pub fn with_cache_responses(mut self, cache_responses: bool) -> Self {
        self.cache_responses = cache_responses;
        self
    }

    pub async fn set_peers(&self, peers: Vec<String>) {
        let local = self.endpoint.local_peer_id();
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for peer in peers {
            let peer = peer.trim().to_string();
            if peer.is_empty() || Some(peer.as_str()) == local.as_deref() || !seen.insert(peer.clone()) {
                continue;
            }
            out.push(peer);
        }
        *self.peers.write().await = out;
    }

    pub fn start(self: &Arc<Self>) -> JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            while let Some(packet) = this.endpoint.recv().await {
                let _ = this.handle_packet(packet).await;
            }
        })
    }

    pub async fn get_from_peers(
        &self,
        hash: &Hash,
        peers: &[String],
    ) -> Result<Option<Vec<u8>>, FipsTransportError> {
        if let Some(data) = self.local_store.get(hash).await? {
            if verify_hash(&data, hash) {
                return Ok(Some(data));
            }
        }
        if peers.is_empty() {
            return Ok(None);
        }

        let key = hash_key(hash);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .await
            .entry(key.clone())
            .or_default()
            .push(PendingRequest { resolve: tx });

        let payload = encode_request(hash, self.request_htl)?;
        let mut sent = 0usize;
        for peer in peers {
            if self.endpoint.send(peer, payload.clone()).await.is_ok() {
                sent += 1;
            }
        }
        if sent == 0 {
            self.resolve_pending(&key, None).await;
            return Ok(None);
        }

        match timeout(self.request_timeout, rx).await {
            Ok(Ok(result)) => Ok(result),
            _ => {
                self.remove_pending_sender(&key).await;
                Ok(None)
            }
        }
    }

    async fn handle_packet(&self, packet: FipsEndpointPacket) -> Result<(), FipsTransportError> {
        let Some(message) = parse_message(&packet.data)? else {
            return Ok(());
        };
        match message {
            Message::Request(req) => {
                let Some(hash) = bytes_to_hash(&req.h) else {
                    return Ok(());
                };
                let Some(data) = self.local_store.get(&hash).await? else {
                    return Ok(());
                };
                if !verify_hash(&data, &hash) {
                    return Ok(());
                }
                self.endpoint
                    .send(&packet.peer_id, encode_response(&hash, &data)?)
                    .await?;
            }
            Message::Response(resp) => {
                let Some(hash) = bytes_to_hash(&resp.h) else {
                    return Ok(());
                };
                if !verify_hash(&resp.d, &hash) {
                    return Ok(());
                }
                if self.cache_responses {
                    let _ = self.local_store.put(hash, resp.d.clone()).await;
                }
                self.resolve_pending(&hash_key(&hash), Some(resp.d)).await;
            }
        }
        Ok(())
    }

    async fn resolve_pending(&self, key: &str, data: Option<Vec<u8>>) {
        let pending = self.pending.lock().await.remove(key);
        if let Some(pending) = pending {
            for request in pending {
                let _ = request.resolve.send(data.clone());
            }
        }
    }

    async fn remove_pending_sender(&self, key: &str) {
        let mut pending = self.pending.lock().await;
        if let Some(requests) = pending.get_mut(key) {
            requests.retain(|request| request.resolve.is_closed());
            if requests.is_empty() {
                pending.remove(key);
            }
        }
    }
}

#[async_trait]
impl<S: Store + Send + Sync + 'static> Store for HashtreeFipsTransport<S> {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.local_store.put(hash, data).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        if let Some(data) = self.local_store.get(hash).await? {
            if verify_hash(&data, hash) {
                return Ok(Some(data));
            }
        }
        let configured = self.peers.read().await.clone();
        let peers = if configured.is_empty() {
            self.endpoint.peer_ids().await
        } else {
            configured
        };
        self.get_from_peers(hash, &peers)
            .await
            .map_err(|err| StoreError::Other(err.to_string()))
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.local_store.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.local_store.delete(hash).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    struct FakeEndpoint {
        id: String,
        network: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<FipsEndpointPacket>>>>,
        rx: Mutex<mpsc::UnboundedReceiver<FipsEndpointPacket>>,
    }

    impl FakeEndpoint {
        async fn new(
            id: &str,
            network: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<FipsEndpointPacket>>>>,
        ) -> Arc<Self> {
            let (tx, rx) = mpsc::unbounded_channel();
            network.lock().await.insert(id.to_string(), tx);
            Arc::new(Self {
                id: id.to_string(),
                network,
                rx: Mutex::new(rx),
            })
        }
    }

    #[async_trait]
    impl FipsEndpointIo for FakeEndpoint {
        async fn send(&self, peer_id: &str, data: Vec<u8>) -> Result<(), FipsTransportError> {
            let tx = self
                .network
                .lock()
                .await
                .get(peer_id)
                .cloned()
                .ok_or_else(|| FipsTransportError::Send(format!("unknown peer {peer_id}")))?;
            tx.send(FipsEndpointPacket {
                peer_id: self.id.clone(),
                data,
            })
            .map_err(|_| FipsTransportError::Send("receiver closed".to_string()))
        }

        async fn recv(&self) -> Option<FipsEndpointPacket> {
            self.rx.lock().await.recv().await
        }

        async fn peer_ids(&self) -> Vec<String> {
            self.network
                .lock()
                .await
                .keys()
                .filter(|id| *id != &self.id)
                .cloned()
                .collect()
        }

        fn local_peer_id(&self) -> Option<String> {
            Some(self.id.clone())
        }
    }

    fn hash(data: &[u8]) -> Hash {
        let digest = Sha256::digest(data);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&digest);
        hash
    }

    #[tokio::test]
    async fn fetches_hash_verified_blob_over_fips_endpoint_bytes() {
        let network = Arc::new(Mutex::new(HashMap::new()));
        let endpoint_a = FakeEndpoint::new("a", network.clone()).await;
        let endpoint_b = FakeEndpoint::new("b", network).await;
        let data = b"hashtree over fips".to_vec();
        let hash = hash(&data);
        let store_a = Arc::new(MemoryStore::new());
        let store_b = Arc::new(MemoryStore::new());
        store_a.put(hash, data.clone()).await.unwrap();

        let transport_a = Arc::new(HashtreeFipsTransport::new(endpoint_a, store_a));
        let transport_b = Arc::new(
            HashtreeFipsTransport::new(endpoint_b, store_b.clone())
                .with_request_timeout(Duration::from_millis(100)),
        );
        transport_a.start();
        transport_b.start();
        transport_b.set_peers(vec!["a".to_string()]).await;

        assert_eq!(transport_b.get(&hash).await.unwrap(), Some(data.clone()));
        assert_eq!(store_b.get(&hash).await.unwrap(), Some(data));
    }

    #[tokio::test(start_paused = true)]
    async fn silence_resolves_unknown_without_retrying_same_peer() {
        let network = Arc::new(Mutex::new(HashMap::new()));
        let endpoint_a = FakeEndpoint::new("a", network.clone()).await;
        let endpoint_b = FakeEndpoint::new("b", network).await;
        let missing = [7u8; 32];
        let transport_a = Arc::new(HashtreeFipsTransport::new(
            endpoint_a,
            Arc::new(MemoryStore::new()),
        ));
        let transport_b = Arc::new(
            HashtreeFipsTransport::new(endpoint_b, Arc::new(MemoryStore::new()))
                .with_request_timeout(Duration::from_millis(25)),
        );
        transport_a.start();
        transport_b.start();
        transport_b.set_peers(vec!["a".to_string()]).await;

        let pending = transport_b.get(&missing);
        tokio::time::advance(Duration::from_millis(30)).await;

        assert_eq!(pending.await.unwrap(), None);
    }
}
